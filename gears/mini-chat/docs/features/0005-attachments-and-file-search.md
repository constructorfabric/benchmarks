---
description: "Specifies the Attachments and File Search feature of the mini-chat gear: document and image upload with validation and per-chat limits, the attachment status lifecycle with synchronous and background indexing into the dedicated per-chat vector store, thumbnails and the Anthropic secondary copy, get and delete of attachments, the file_search, code_interpreter and search_knowledge tool contributions with citation file resolution, the attachment_cleanup outbox handler with the cleanup_status state machine, and the upload reaper."
---

# Feature: Attachments and File Search


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Uploads a Document](#chat-user-uploads-a-document)
  - [Chat User Uploads an Image](#chat-user-uploads-an-image)
  - [Chat User Polls Attachment Status](#chat-user-polls-attachment-status)
  - [Chat User Deletes an Attachment](#chat-user-deletes-an-attachment)
  - [Chat User Asks About Uploaded Files](#chat-user-asks-about-uploaded-files)
  - [Admin Observes Attachment Health and Recovers Cleanup](#admin-observes-attachment-health-and-recovers-cleanup)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Upload Intake](#upload-intake)
  - [Purpose Routing](#purpose-routing)
  - [Per-Chat Limits and Insert](#per-chat-limits-and-insert)
  - [Provider File Upload](#provider-file-upload)
  - [Secondary Copy](#secondary-copy)
  - [Vector Store Provisioning](#vector-store-provisioning)
  - [In-Request Indexing Wait](#in-request-indexing-wait)
  - [Background Indexing](#background-indexing)
  - [Image Thumbnail](#image-thumbnail)
  - [Upload Failure Handling](#upload-failure-handling)
  - [Upload Request Completion](#upload-request-completion)
  - [Attachment Detail Projection](#attachment-detail-projection)
  - [Attachment Delete Transaction](#attachment-delete-transaction)
  - [Attachment Cleanup Enqueue](#attachment-cleanup-enqueue)
  - [Attachment Cleanup Handler](#attachment-cleanup-handler)
  - [Upload Reaper](#upload-reaper)
  - [Ready Document Fact](#ready-document-fact)
  - [Ready Code Interpreter Fact](#ready-code-interpreter-fact)
  - [File Search Contribution](#file-search-contribution)
  - [Code Interpreter Contribution](#code-interpreter-contribution)
  - [Knowledge Search Contribution](#knowledge-search-contribution)
  - [Knowledge Search Loop](#knowledge-search-loop)
  - [Attachment Error Contract](#attachment-error-contract)
  - [Attachment Metrics](#attachment-metrics)
  - [Attachment Configuration](#attachment-configuration)
- [4. States (CDSL)](#4-states-cdsl)
  - [Attachment Status State Machine](#attachment-status-state-machine)
  - [Attachment Cleanup Status State Machine](#attachment-cleanup-status-state-machine)
  - [Secondary Copy State Machine](#secondary-copy-state-machine)
  - [Chat Vector Store State Machine](#chat-vector-store-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Upload Endpoint, Intake and Purpose Routing](#upload-endpoint-intake-and-purpose-routing)
  - [Per-Chat Limits and Provider File Upload](#per-chat-limits-and-provider-file-upload)
  - [Per-Chat Vector Store](#per-chat-vector-store)
  - [Indexing Lifecycle With Deadline and Background Continuation](#indexing-lifecycle-with-deadline-and-background-continuation)
  - [Images, Thumbnails and the Anthropic Secondary Copy](#images-thumbnails-and-the-anthropic-secondary-copy)
  - [Get Attachment API](#get-attachment-api)
  - [Delete Attachment and Mutability](#delete-attachment-and-mutability)
  - [Attachment Cleanup Handler and Cleanup State Machine](#attachment-cleanup-handler-and-cleanup-state-machine)
  - [Upload Reaper Recovery](#upload-reaper-recovery)
  - [Tool Contributions, Retrieval Scope and Citation File Map](#tool-contributions-retrieval-scope-and-citation-file-map)
  - [Knowledge Search Agentic Loop](#knowledge-search-agentic-loop)
  - [Error Contract, Observability and Configuration](#error-contract-observability-and-configuration)
  - [Deferred and Not-Implemented Scope Stays Unimplemented](#deferred-and-not-implemented-scope-stays-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-attachments-and-file-search-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-attachments-and-file-search`

<!--
=============================================================================
FEATURE SPECIFICATION: Attachments and File Search (mini-chat)
Source entry: DECOMPOSITION section 2.5 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml, over the extracted OpenAPI document and over platform documents
under docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Attachments and File Search feature lets a chat user add documents and images to a chat and ask questions about them. It covers the three attachment routes (upload, get, delete), upload-time validation (MIME type, per-file size, kill switches, code-interpreter routing, per-chat document and storage limits), the provider file upload through OAGW, the dedicated per-chat vector store, synchronous indexing with a 25 s request deadline and a bounded background continuation, image thumbnails and the Anthropic secondary copy, the tool contributions (`file_search`, `code_interpreter`, `search_knowledge`) that plug into the send pipeline of `cpt-cf-mini-chat-feature-message-streaming`, citation file resolution, attachment deletion with the mutability rule, the single `attachment_cleanup` outbox handler with the attachment `cleanup_status` state machine, and the upload reaper that recovers abandoned uploads.

### 1.2 Purpose

The PRD requires users to ground conversations in their own documents and images, with bounded retrieval cost per chat, owner-only and tenant-isolated access, asynchronous provider cleanup on deletion and no provider identifier ever reaching a client. The DESIGN fixes the synchronous upload with a 25 s indexing deadline and background continuation (ADR-0007), one provider vector store per chat created lazily under a compare-and-set protocol, retrieval over the entire chat vector store, the transactional-outbox deletion path, the `cleanup_status` state machine and the leader-elected upload reaper. This feature realizes those contracts and owns every write of `attachments` and `chat_vector_stores` outside chat deletion.

Scope boundary: validation of `attachment_ids` in the send pipeline (same tenant, uploader and chat, status `ready`, unique, count cap), the send-time image guards (`disable_images`, `VISION_INPUT` on the effective model, `TOO_MANY_IMAGES`), the generic tool assembly, the provider request construction (including the `input_image` items and the Anthropic image blocks), tool event relay and accounting and the citation mapping are owned by `cpt-cf-mini-chat-feature-message-streaming`; this feature supplies the tool contributions, the attachment facts, the citation file map and the provider file ids they consume. The tables, the outbox queues (including the registration of `mini-chat.attachment_cleanup`), the configuration sections, OAGW provisioning, the leader elector and request admission are owned by `cpt-cf-mini-chat-feature-gear-foundation`. The daily code interpreter quota and the reserve surcharges are owned by `cpt-cf-mini-chat-feature-quota-and-billing`. The chat-cleanup handler and the provider vector store deletion on chat deletion belong to `cpt-cf-mini-chat-feature-background-maintenance`, which applies the `cleanup_status` transitions defined here. Retry and edit (copying attachment links) belong to `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`.

**Requirements**: `cpt-cf-mini-chat-fr-file-upload`, `cpt-cf-mini-chat-fr-image-upload`, `cpt-cf-mini-chat-fr-file-search`, `cpt-cf-mini-chat-fr-code-interpreter`, `cpt-cf-mini-chat-fr-per-chat-doc-limits`, `cpt-cf-mini-chat-fr-attachment-deletion`, `cpt-cf-mini-chat-fr-doc-summary` (deferred, not implemented; ADR-0007), `cpt-cf-mini-chat-nfr-rag-scalability`

**Principles**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content` (preserved through the foundation's scoped persistence; no principle is specific to attachments)

**Design elements**: `cpt-cf-mini-chat-design-vector-store-scope`, `cpt-cf-mini-chat-design-rag-quality-controls`, `cpt-cf-mini-chat-design-code-interpreter`, `cpt-cf-mini-chat-design-code-interpreter-config`, `cpt-cf-mini-chat-seq-file-upload`. Consumed: `cpt-cf-mini-chat-component-llm-provider` (file storage and vector store dispatch by `storage_kind`, the Anthropic Files client, the knowledge retriever), `cpt-cf-mini-chat-interface-public-api` (the three attachment routes), `cpt-cf-mini-chat-seq-chat-deletion-cleanup` (ownership transfer of attachment cleanup to chat cleanup).

**Database tables used (created by `cpt-cf-mini-chat-feature-gear-foundation`; this feature owns their lifecycle behavior)**: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`. Read only: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-message-attachments` (deletion lock). Written through a guarded counter update during knowledge search: `cpt-cf-mini-chat-dbtable-chat-turns` (`file_search_completed_count`).

**Domain entities**: `Attachment` (including its cleanup status and secondary copy), `ChatVectorStore`, the attachment cleanup outbox message.

**ADRs traced**: `cpt-cf-mini-chat-adr-document-retrieval-scope` (synchronous upload with deadline and background continuation, no document summary, no chunk cap, `max_tool_calls` instead of a per-turn `file_search` limit, no per-user daily `file_search` limit, no immediate retrieval exclusion on delete, no Anthropic document search, messages list only non-deleted attachments), `cpt-cf-mini-chat-adr-multi-provider-adapters` (storage dispatch by `storage_kind` and `rag_provider`, Anthropic secondary image copy, tool support per adapter), `cpt-cf-mini-chat-adr-canonical-error-contract` (upload, get and delete error mapping), `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (soft-deleted attachments and thumbnails are never hard-purged; provider files are removed by outbox handlers), `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (no CHECK on `cleanup_status`, application clock, deprecated `cleanup_worker` keys), `cpt-cf-mini-chat-adr-quota-policy-scope` (no daily image quota, knowledge-search iteration billing not implemented).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Uploads documents, spreadsheets and images to an owned chat, polls an attachment until it is `ready` or `failed`, deletes an attachment that no sent message references, and asks questions that use the chat's documents, spreadsheets and explicitly attached images. |
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | Runs as the `attachment_cleanup` outbox handler (provider file deletion for deleted attachments, abandoned uploads and failed background indexing) and as the leader-elected upload reaper; no human interaction. |
| `cpt-cf-mini-chat-actor-admin` | Observes upload, indexing, reaper, cleanup and knowledge-search health through low-cardinality metrics and replays dead-lettered cleanup messages through the shared outbox tooling (a replay of a message whose row is `failed` re-attempts the provider file delete once); never sees file content or provider identifiers in metric labels. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) section 5.2 (Document Support), use cases `cpt-cf-mini-chat-usecase-upload-document`, `cpt-cf-mini-chat-usecase-upload-image`, `cpt-cf-mini-chat-usecase-doc-search`; section 13 (resolved MIME allowlist)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.5 (`Attachments and File Search`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md) section `Attachments`
- **ADRs (all accepted)**: [ADR-0004](../ADR/0004-cpt-cf-mini-chat-adr-canonical-error-contract.md), [ADR-0005](../ADR/0005-cpt-cf-mini-chat-adr-multi-provider-adapters.md), [ADR-0007](../ADR/0007-cpt-cf-mini-chat-adr-document-retrieval-scope.md), [ADR-0008](../ADR/0008-cpt-cf-mini-chat-adr-quota-policy-scope.md), [ADR-0009](../ADR/0009-cpt-cf-mini-chat-adr-data-lifecycle-audit-scope.md), [ADR-0010](../ADR/0010-cpt-cf-mini-chat-adr-runtime-consistency-limitations.md)
- **Reference OpenAPI** (operations `mini_chat.upload_attachment`, `mini_chat.get_attachment`, `mini_chat.delete_attachment`; schemas `AttachmentDetailDto`, `AttachmentKindDto`, `AttachmentStatusDto`, `AttachmentSummaryDto`, `ImgThumbnailDto`): `.cf-studio/.cache/mini-chat/openapi-mini-chat.json`; where it differs from the DESIGN, the DESIGN governs
- **Cross-feature implementation notes**: `.cf-studio/.cache/mini-chat/implementation-notes.md` items 1 and 2 (SQLite timestamp ordering and comparison, used by the reaper cutoff and its oldest-first order), 3 (repository functions take a runner so the domain write and the outbox enqueue share one transaction; the outbox wake fires after commit), 4 (PEP and 404 masking), 5 (every OAGW call under the gear's S2S context with the `/{upstream_alias}{api_path}` URI form), 6 (domain layering), 7 (the attachment cleanup queue still carries the placeholder `UnwiredHandler`, which this feature replaces; global outbox tuning with batch 1 and a 30 s retry cap)
- **Sibling features**: [0001-gear-foundation.md](./0001-gear-foundation.md) (request admission, authorization matrix actions `upload_attachment`, `read_attachment`, `delete_attachment`, canonical error mapping, scoped persistence, schema, outbox queues and transactional enqueue, policy snapshot access, provider registry, OAGW provisioning with the RAG routes, leader elector, configuration validation), [0002-chat-management.md](./0002-chat-management.md) (chat deletion marks attachments `cleanup_status` `pending`), [0003-quota-and-billing.md](./0003-quota-and-billing.md) (file search and code interpreter surcharges, daily code interpreter quota), [0004-message-streaming.md](./0004-message-streaming.md) (attachment validation, image guards, tool assembly, request construction, tool accounting, citation mapping, turn finalization)
- **Dependencies**: `cpt-cf-mini-chat-feature-message-streaming` (and through it `cpt-cf-mini-chat-feature-gear-foundation`, `cpt-cf-mini-chat-feature-chat-management`, `cpt-cf-mini-chat-feature-quota-and-billing`). Features that build on this one: `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` (carries attachment links forward), `cpt-cf-mini-chat-feature-background-maintenance` (chat-cleanup handler applies the `cleanup_status` transitions).

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest):

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Upload, get and delete contracts, upload error table | 3.3 `Get Attachment`, `Upload Attachment`; REST error mapping under `Provider Event Translation` (`Error Codes`) |
| Upload sequence, purposes, indexing deadline, background continuation, thumbnails, attachment status | 3.6 `File Upload` |
| Cleanup boundaries, `cleanup_status` state machine, vector store cleanup ordering | 3.6 `Cleanup on Chat Deletion` (`Cleanup responsibility boundaries`, `Attachment cleanup state machine`, `Vector store cleanup ordering`, `Additional invariants`) |
| `attachments`, `message_attachments`, `chat_vector_stores` columns, indexes, creation protocol | 3.7 `Table: attachments`, `Table: message_attachments`, `Table: chat_vector_stores` |
| Storage dispatch, Files API field mapping, file storage, Anthropic secondary copy | 2.2 `OpenAI-Compatible Provider (P1)`; 3.2 `llm_provider` |
| Tool availability, retrieval scope and invariants, RAG assembly, citation resolution, mutability, deletion, scale controls, call limits, knowledge search | 4 `File Search Tool Availability`, `Code Interpreter Tool Availability`, `Code Interpreter Configuration`, `File Search Retrieval Scope`, `Retrieval Invariants`, `Vector Store Scope (P1)`, `RAG Context Assembly Rules`, `Citation File ID and Title Resolution`, `Attachment Mutability and Deletion`, `Attachment Deletion`, `RAG Quality & Scale Controls (P1)`, `File Search Per-Turn Call Limit Enforcement (P1)`, `Knowledge Search` |
| Metrics | 4 `Metrics (OTLP) and Alerts (P1)` (areas Upload reaper, Attachments, Knowledge search, Uploads, Images, Cleanup) and 3.6 `Observability (P1)` |
| Configuration keys and defaults | Appendix B `B.2.3`, `B.6.1`, `B.7`, `B.8`, `B.9.2`, `B.9.3`, `B.9.5` |

## 2. Actor Flows (CDSL)

User-facing interactions that start with the chat user and describe how the attachment routes and the attachment-aware turn behave end to end. Every flow runs behind the request admission flow `cpt-cf-mini-chat-flow-gear-foundation-request-admission` (authentication, license gate, PEP with the action of the authorization matrix, owner-scoped chat load and 404 masking).

**Use cases**: `cpt-cf-mini-chat-usecase-upload-document`, `cpt-cf-mini-chat-usecase-upload-image`, `cpt-cf-mini-chat-usecase-doc-search` (its streaming steps run through `cpt-cf-mini-chat-flow-message-streaming-send-message`).

### Chat User Uploads a Document

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A document routed to `file_search` is uploaded to the RAG provider, added to the chat's vector store (created on the first document upload) and indexed within 25 s of the request start: HTTP 201 with `AttachmentDetail` in `status` `ready`.
- The vector store still reports the file `in_progress` 25 s after the request started: HTTP 201 with `status` `uploaded`; a background task finishes indexing within 10 minutes and the client polls the get flow.
- An XLSX file routed only to `code_interpreter` is uploaded to the provider and not added to the vector store: HTTP 201 with `status` `ready`.

**Error Scenarios**:
- Before the body is read: 401, 403 (`LICENSE_FEATURE_REQUIRED`, `AUTHZ_DENIED`), 503 with `Retry-After: 5` from admission; 404 `not_found` with the chat `resource_type` for a missing, soft-deleted or foreign chat; 400 `invalid_path_params`; 400 `INVALID_MODEL` when the chat's model left the catalog; 500 `internal` when the policy or storage provider cannot be resolved; 503 with `Retry-After: 5` when the per-process upload concurrency limit is reached.
- Request shape: 400 `BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`, `MISSING_CONTENT_TYPE`, `UNSUPPORTED_CONTENT_TYPE`, `CODE_INTERPRETER_UNAVAILABLE`; 413 from api-gateway for a body above `defaults.body_limit_bytes`.
- Limits: 429 `resource_exhausted` with subject `document_limit` or `storage_limit`; 400 `out_of_range` `FILE_TOO_LARGE`.
- Provider and storage: 503 `service_unavailable` with `Retry-After: 10` (upload, vector store or indexing failure within the deadline); 409 `already_exists` `provider_mismatch`. After the row was inserted the row stays visible with `status` `failed` and an `error_code`.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/attachments` as `multipart/form-data` with one `file` part - `inst-doc-request`
2. [x] - `p1` - Record the request start instant on the application clock; the indexing deadline is 25 s after it - `inst-doc-start-instant`
3. [x] - `p1` - Run request admission with the action `upload_attachment` and the chat id as `resource.id`, which loads the chat through the owner-scoped query - `inst-doc-admission`
4. [x] - `p1` - **IF** admission returned a refusal or no chat - `inst-doc-refused`
   1. [x] - `p1` - **RETURN** 401, 403, 503 with `Retry-After: 5`, or 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.chat.v1~`, before any body byte is read - `inst-doc-return-refusal`
5. [x] - `p1` - Run the upload intake algorithm (model resolution, storage provider resolution, concurrency permit, multipart headers, MIME type, filename, kind, purpose routing, effective size limit) - `inst-doc-intake`
6. [x] - `p1` - **IF** the intake rejected the request - `inst-doc-intake-rejected`
   1. [x] - `p1` - **RETURN** the intake Problem; no row was written and no file byte was read; when the intake had already taken the concurrency permit it ran the upload request completion algorithm before returning, so the permit is released and the upload metric recorded - `inst-doc-return-intake`
7. [x] - `p1` - Permit rule (flow level): from here on the request holds the upload concurrency permit, and every **RETURN** below (429 limit, in-request failure, a database failure mapped to 500 `internal`, 201 `uploaded` and 201 `ready`) first runs the upload request completion algorithm, which releases the permit exactly once and records `mini_chat_attachment_upload{kind, result}` once, `result` `ok` for each 201 including `uploaded` and `error` for every Problem; the background indexing task never holds a permit - `inst-doc-permit-rule`
8. [x] - `p1` - Failure rule (flow level): a failure reported by the provider file upload step or by any sub-step of the file search path (byte limit, provider upload, storage re-check, vector store, provider mismatch, in-request indexing) transfers at once to the in-request failure handling (`inst-doc-failed`), and no later step of this flow runs - `inst-doc-failure-rule`
9. [x] - `p1` - Run the per-chat limits and insert algorithm, which inserts the `pending` row - `inst-doc-insert`
10. [x] - `p1` - **IF** a per-chat limit rejected the upload - `inst-doc-limit-rejected`
    1. [x] - `p1` - Run the upload request completion algorithm with the outcome `error` and **RETURN** 429 `resource_exhausted` with subject `document_limit` or `storage_limit`; no row was inserted - `inst-doc-return-limit`
11. [x] - `p1` - Run the provider file upload algorithm, which streams the bytes to the storage provider with a byte counter and moves the row to `uploaded` - `inst-doc-provider-upload`
12. [x] - `p1` - **IF** `for_file_search` is set - `inst-doc-file-search-path`
    1. [x] - `p1` - Run the vector store provisioning algorithm to get or create the chat's vector store - `inst-doc-vector-store`
    2. [x] - `p1` - Run the in-request indexing wait algorithm with the request start instant - `inst-doc-indexing-wait`
    3. [x] - `p1` - **IF** the indexing wait reported the file still `in_progress` at the deadline - `inst-doc-still-indexing`
       1. [x] - `p1` - Spawn the background indexing algorithm for the row and leave the row `uploaded` - `inst-doc-spawn-background`
       2. [x] - `p1` - Run the upload request completion algorithm with the outcome `ok`, which releases the permit before the response is returned although the background task continues - `inst-doc-complete-uploaded`
       3. [x] - `p1` - **RETURN** 201 with the attachment detail projection in `status` `uploaded` - `inst-doc-return-uploaded`
13. [x] - `p1` - **IF** any step after the insert failed (byte limit, provider upload, storage re-check, vector store, provider mismatch, indexing; the failure rule `inst-doc-failure-rule` routes the steps 11 and 12 failures here) - `inst-doc-failed`
    1. [x] - `p1` - Run the upload failure algorithm, which also runs the upload request completion algorithm with the outcome `error`, and **RETURN** its Problem (400 `FILE_TOO_LARGE`, 429 `storage_limit`, 409 `provider_mismatch` or 503 with `Retry-After: 10`) - `inst-doc-return-failure`
14. [x] - `p1` - Leave a document routed only to `code_interpreter` out of the vector store; its provider file id is offered to the tool at stream time by the code interpreter contribution algorithm - `inst-doc-ci-path`
15. [x] - `p1` - DB: UPDATE the `attachments` row to `status` `ready` and `updated_at` now, by `id` and `tenant_id` - `inst-doc-set-ready`
16. [x] - `p1` - Run the upload request completion algorithm with the outcome `ok`, which releases the permit and records the upload metrics - `inst-doc-metrics`
17. [x] - `p1` - **RETURN** 201 with the attachment detail projection in `status` `ready` - `inst-doc-return-ready`

### Chat User Uploads an Image

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A PNG, JPEG, WebP or GIF image is uploaded synchronously to the RAG provider, never to a vector store, a WebP thumbnail is generated when the safeguards allow it, and the response is HTTP 201 with `status` `ready` and `img_thumbnail` (or without it when generation was skipped).
- In a chat whose model is served by an `anthropic_messages` provider, the image also gets a secondary copy in the Anthropic Files API, which the Anthropic adapter of `cpt-cf-mini-chat-feature-message-streaming` references.

**Error Scenarios**:
- The admission, intake, limit and provider errors of the document flow (images have no vector store path, so no `provider_mismatch` and no indexing failure).
- `disable_images` on: 400 `failed_precondition` with `violations[{subject: images, type: FEATURE_DISABLED}]` before any body byte is read. No vision check runs at upload; `VISION_NOT_SUPPORTED` is a send-time guard of `cpt-cf-mini-chat-algo-message-streaming-image-guards`.
- A thumbnail or secondary-copy failure is not an upload failure.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/attachments` with an image in the `file` part - `inst-img-request`
2. [x] - `p1` - Run request admission with the action `upload_attachment` and **RETURN** its refusal or 404 for the chat type before any body byte is read - `inst-img-admission`
3. [x] - `p1` - Run the upload intake algorithm, which rejects images while `disable_images` is on and resolves the kind `image` with both purpose flags `false` - `inst-img-intake`
4. [x] - `p1` - **IF** the intake rejected the request - `inst-img-intake-rejected`
   1. [x] - `p1` - **RETURN** the intake Problem (400 `FEATURE_DISABLED` subject `images`, 400 `UNSUPPORTED_CONTENT_TYPE`, or another intake Problem); when the intake had already taken the concurrency permit it ran the upload request completion algorithm before returning, so the permit is released and the upload metric recorded - `inst-img-return-intake`
5. [x] - `p1` - Permit rule (flow level): from here on the request holds the upload concurrency permit, and every **RETURN** below (429 limit, upload failure, a database failure mapped to 500 `internal` and 201 `ready`) first runs the upload request completion algorithm, which releases the permit exactly once and records `mini_chat_attachment_upload{kind, result}` once, `result` `ok` for the 201 and `error` for every Problem - `inst-img-permit-rule`
6. [x] - `p1` - Run the per-chat limits and insert algorithm; images count toward the storage limit only, never toward the document limit - `inst-img-insert`
7. [x] - `p1` - **IF** the storage limit rejected the upload - `inst-img-limit-rejected`
   1. [x] - `p1` - Run the upload request completion algorithm with the outcome `error` and **RETURN** 429 `resource_exhausted` with subject `storage_limit` - `inst-img-return-limit`
8. [x] - `p1` - Run the provider file upload algorithm, keeping the image bytes in memory only while their count stays at or below `thumbnail.max_decode_bytes` - `inst-img-provider-upload`
9. [x] - `p1` - **IF** the provider upload, the byte limit or the storage re-check failed - `inst-img-upload-failed`
   1. [x] - `p1` - Run the upload failure algorithm, which also runs the upload request completion algorithm with the outcome `error`, and **RETURN** its Problem - `inst-img-return-failure`
10. [x] - `p1` - Run the secondary copy algorithm (Anthropic chats only) - `inst-img-secondary`
11. [x] - `p1` - Run the thumbnail algorithm synchronously - `inst-img-thumbnail`
12. [x] - `p1` - DB: UPDATE the row to `status` `ready` with `img_thumbnail`, `img_thumbnail_width` and `img_thumbnail_height` set from the thumbnail result or left NULL when it was skipped, and `updated_at` now; `error_code` stays NULL - `inst-img-set-ready`
13. [x] - `p1` - Never add the image to a vector store; it becomes model input only on a turn that lists it in `attachment_ids` - `inst-img-no-index`
14. [x] - `p1` - Run the upload request completion algorithm with the outcome `ok`, which releases the permit and records the upload metrics - `inst-img-metrics`
15. [x] - `p1` - **RETURN** 201 with the attachment detail projection in `status` `ready` - `inst-img-return-ready`

### Chat User Polls Attachment Status

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- HTTP 200 with `AttachmentDetail`: `id`, `filename`, `content_type`, `size_bytes`, `status`, `kind`, `created_at`, `error_code` only for `failed`, `img_thumbnail` only for a `ready` image with a stored thumbnail; `doc_summary` and `summary_updated_at` never present.
- After an upload returned `uploaded`, repeated polls show `uploaded` until the background task sets `ready` or `failed` (`indexing_failed`), or the reaper sets `failed` (`upload_abandoned`).

**Error Scenarios**:
- 401, 403, 503 from admission; 404 for the chat type; 400 `invalid_path_params` for a non-UUID id.
- 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.attachment.v1~` for an unknown, soft-deleted, other-chat or other-uploader attachment.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/chats/{id}/attachments/{attachment_id}` - `inst-get-request`
2. [x] - `p1` - Run request admission with the action `read_attachment` and the chat id as `resource.id`; **RETURN** its refusal or 404 for the chat type - `inst-get-admission`
3. [x] - `p1` - DB: SELECT the `attachments` row by `id`, `tenant_id` and `chat_id` of the scoped chat with `deleted_at IS NULL` - `inst-get-select`
4. [x] - `p1` - **IF** no row exists or `uploaded_by_user_id` differs from the caller - `inst-get-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.attachment.v1~`, the same response for all four cases - `inst-get-return-404`
5. [x] - `p1` - **RETURN** 200 with the attachment detail projection - `inst-get-return-200`

### Chat User Deletes an Attachment

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- An attachment that no message references is soft-deleted and one `attachment_deleted` cleanup message is enqueued in the same transaction: HTTP 204. It disappears at once from chat metadata, from the `file_search` inclusion check and from citations; its chunks can still be returned by `file_search` until the provider file is deleted (ADR-0007).
- A repeated delete of the same attachment by its uploader returns 204 without a second cleanup message.

**Error Scenarios**:
- 401, 403, 503 from admission; 404 for the chat type; 400 `invalid_path_params`.
- 404 `not_found` with the attachment `resource_type` for an unknown attachment, an attachment of another chat, or an attachment uploaded by another user (also on a repeated delete).
- 409 `already_exists` with `resource_name` `attachment_locked` when any message references the attachment.
- 500 `internal` when the cleanup payload exceeds the outbox size limit or the transaction fails; nothing is changed.

**Steps**:
1. [x] - `p1` - Chat User sends `DELETE /v1/chats/{id}/attachments/{attachment_id}` - `inst-del-request`
2. [x] - `p1` - Run request admission with the action `delete_attachment` and the chat id as `resource.id`; **RETURN** its refusal or 404 for the chat type - `inst-del-admission`
3. [x] - `p1` - Run the attachment delete transaction algorithm - `inst-del-transaction`
4. [x] - `p1` - **MATCH** the result - `inst-del-match`
   - [x] - `p1` - **CASE** not found or other uploader: **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.attachment.v1~` - `inst-del-case-404`
   - [x] - `p1` - **CASE** already deleted: **RETURN** 204 without enqueueing anything - `inst-del-case-idempotent`
   - [x] - `p1` - **CASE** locked: **RETURN** 409 `already_exists` with `resource_name` `attachment_locked` - `inst-del-case-locked`
   - [x] - `p1` - **CASE** payload too large or database failure: **RETURN** 500 `internal` with a generic detail - `inst-del-case-500`
   - [x] - `p1` - **CASE** deleted: **RETURN** 204 No Content without waiting for any provider call - `inst-del-case-204`

### Chat User Asks About Uploaded Files

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- In a chat with at least one `ready` document routed to `file_search`, every turn on a model whose `tool_support.file_search` is set (and with `disable_file_search` off) carries the `file_search` tool over the entire chat vector store; the provider decides whether to call it, `tool` events named `file_search` are relayed, and file citations carry `attachment_id` and the filename as `title`.
- In a chat with a `ready` code-interpreter attachment, the turn carries `code_interpreter` with the attachment file ids when the effective model supports it and `disable_code_interpreter` is off.
- An image listed in `attachment_ids` is model input for that turn only.
- With knowledge search enabled and no `file_search` in the turn, the model may call `search_knowledge`, which the gear answers from the organization knowledge base.

**Error Scenarios**:
- Send-time rejections (invalid, foreign, not-ready or deleted `attachment_ids`, image guards, daily code interpreter quota) are Problems of `cpt-cf-mini-chat-feature-message-streaming` and `cpt-cf-mini-chat-feature-quota-and-billing`.
- A knowledge-search loop that exceeds its iteration cap ends the stream with SSE `error{code: "agentic_iterations_exceeded"}`.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats/{id}/messages:stream` with `content` and optionally `attachment_ids`; the send flow `cpt-cf-mini-chat-flow-message-streaming-send-message` runs - `inst-ask-send`
2. [x] - `p1` - The send preflight reads the chat facts through the ready document fact algorithm (`cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`) and the ready code interpreter fact algorithm (`cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact`) of this feature, so the reserve surcharges and the daily code interpreter check of `cpt-cf-mini-chat-feature-quota-and-billing` match the tools that are sent; an XLSX-only chat reserves no `file_search` surcharge and sends no `file_search` tool - `inst-ask-facts`
3. [x] - `p1` - The tool assembly `cpt-cf-mini-chat-algo-message-streaming-tool-assembly` asks the file search, code interpreter and knowledge search contribution algorithms of this feature, in that order of evaluation, for their tool definitions, guards and `include` entries - `inst-ask-contributions`
4. [x] - `p1` - The reserve transaction validates `attachment_ids` through `cpt-cf-mini-chat-algo-message-streaming-attachment-validation`; `attachment_ids` never scopes or filters retrieval in P1 - `inst-ask-validation`
5. [x] - `p1` - The provider request built by `cpt-cf-mini-chat-algo-message-streaming-responses-request` references images by their internal `provider_file_id` (or, on the Anthropic adapter, by the `uploaded` secondary file id) and carries the contributed tools; the client never sends and never sees a provider identifier - `inst-ask-request`
6. [x] - `p1` - **IF** knowledge search is active for the turn and the model calls `search_knowledge` - `inst-ask-knowledge`
   1. [x] - `p1` - Run the knowledge search loop algorithm - `inst-ask-knowledge-loop`
7. [x] - `p1` - On a normally completed stream, the citation mapping `cpt-cf-mini-chat-algo-message-streaming-citations` resolves file annotations through the citation file map built by the file search contribution; unknown and soft-deleted attachments are omitted - `inst-ask-citations`
8. [x] - `p1` - **RETURN** the streamed response of the send flow - `inst-ask-return`

### Admin Observes Attachment Health and Recovers Cleanup

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability`

**Actor**: `cpt-cf-mini-chat-actor-admin`

**Success Scenarios**:
- The admin sees upload outcomes per kind, background indexing outcomes, abandoned uploads per source status, reaper scan durations, cleanup completions, retries and terminal failures, skipped secondary deletions and knowledge-search outcomes, all without tenant, user, chat, attachment or provider identifiers in labels.
- A dead-lettered `attachment_cleanup` message is replayed through the shared outbox dead-letter tooling; the handler processes it against the current row state. When the row's `cleanup_status` is `failed`, the replay re-attempts the provider file delete once: a 2xx or 404 response sets the row `done` (provider-file debt cleared), and a failure leaves the row `failed` and dead-letters the message again, so a replay never loops.
- A replay of a message whose row is already `done`, whose row is missing or whose parent chat is soft-deleted is acknowledged without any action.

**Error Scenarios**:
- A row whose cleanup reached `failed` is not re-enqueued automatically; it stays as provider-file debt until an operator replays its dead-lettered message (P1 limitation, DESIGN 3.6 invariant 3 `operator intervention`).
- A replay while the provider still fails: the row stays `failed`, `cleanup_attempts` does not grow and the message returns to the dead-letter store for a later replay.

**Steps**:
1. [x] - `p1` - Admin reads the metric series of the attachment metrics algorithm from the OTLP pipeline - `inst-adm-read-metrics`
2. [x] - `p1` - **IF** `mini_chat_cleanup_failed{resource_type="file"}` grows - `inst-adm-cleanup-failed`
   1. [x] - `p1` - Admin inspects the dead-lettered `attachment_cleanup` messages in the shared outbox dead-letter store and the `last_cleanup_error` of the rows, which hold no file content - `inst-adm-inspect`
   2. [x] - `p1` - Admin replays a dead-lettered message after the provider issue is fixed; for a `failed` row the handler re-attempts the provider delete once and sets `done` on success or leaves `failed` and rejects again on failure, and it acks the message without action when the row is `done` or missing or the parent chat is soft-deleted - `inst-adm-replay`
3. [x] - `p1` - **RETURN** the observed state; no admin route is added by this feature - `inst-adm-return`

## 3. Processes / Business Logic (CDSL)

Internal algorithms of the upload, indexing, deletion, cleanup, reaper and tool-contribution paths. All provider calls go through the in-process OAGW client `ServiceGatewayClientV1` with the URI `/{upstream_alias}{path}` under the gear's S2S security context (implementation note 5); OAGW injects credentials (`cpt-cf-mini-chat-constraint-no-credentials`). File and vector store paths use the prefix `/v1` for `storage_kind` `openai` and `/openai` with the `api-version` query parameter for `azure`, matching the RAG routes provisioned by `cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning`.

### Upload Intake

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake`

**Input**: the scoped chat, the security context, the request headers and the multipart stream positioned before the `file` part body

**Output**: the intake result (chat model entry, storage provider, concurrency permit, filename, MIME type, kind, purposes, effective per-file byte limit) or a Problem; once the permit is taken, a Problem is returned only after the permit was released (the permit rule below). No file byte is read and no row is written by this algorithm (DESIGN 3.3 `Upload Attachment`, B.8).

**Steps**:
1. [x] - `p1` - Resolve the chat's model (`chats.model`) in the current catalog without the enabled filter through `cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access`; there is no quota cascade on upload - `inst-intake-model`
2. [x] - `p1` - **IF** the model is no longer in the catalog - `inst-intake-model-missing`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[model].reason` `INVALID_MODEL` - `inst-intake-return-invalid-model`
3. [x] - `p1` - **IF** the model-policy plugin fails or another resolver error occurs - `inst-intake-policy-failed`
   1. [x] - `p1` - **RETURN** the resolver error as is (500 `internal` for a plugin failure); there is no fallback provider and no fallback limit - `inst-intake-return-policy`
4. [x] - `p1` - Resolve the storage provider through the provider registry: the `rag_provider` of the model's provider entry, or that entry itself when `rag_provider` is not set, with its `storage_kind` (`openai` or `azure`), its upstream alias for the caller's tenant and its `storage_backend` label (`providers.<id>.storage_backend`, or the provider id) - `inst-intake-storage`
5. [x] - `p1` - **IF** no storage-capable provider resolves - `inst-intake-storage-missing`
   1. [x] - `p1` - **RETURN** 500 `internal` with a generic detail - `inst-intake-return-storage`
6. [x] - `p1` - Try to take one permit of the per-process upload semaphore sized `rag.max_concurrent_uploads` (default 10) without waiting; the permit is held until the upload request completion algorithm releases it on the request's return path and is never held by the background indexing task - `inst-intake-permit`
7. [x] - `p1` - **IF** no permit is free - `inst-intake-permit-busy`
   1. [x] - `p1` - **RETURN** 503 `service_unavailable` with `Retry-After: 5` and `context.retry_after_seconds` 5; no permit is held, so nothing is released and no upload metric is recorded - `inst-intake-return-busy`
8. [x] - `p1` - Permit rule: from here on this request holds a permit, so every Problem returned by the steps below (multipart, filename, MIME type, kind, `disable_images`, purpose routing) first runs the upload request completion algorithm with the outcome `error`, which releases the permit and records the upload metric; on success the permit passes to the calling flow, which releases it through the same algorithm - `inst-intake-permit-rule`
9. [x] - `p1` - Parse the multipart headers: **RETURN** 400 `invalid_argument` with `BOUNDARY_REQUIRED` on field `content_type` when the `Content-Type` has no boundary, `MULTIPART_ERROR` on field `multipart` when the body is unreadable, `MISSING_FILE` on field `file` when no `file` part exists, and `MISSING_CONTENT_TYPE` on field `content_type` when the `file` part has no content type - `inst-intake-multipart`
10. [x] - `p1` - Normalize the filename: `upload` when the part has none, truncated to 255 characters while keeping the extension - `inst-intake-filename`
11. [x] - `p1` - Resolve the MIME type from the part content type; when it is `application/octet-stream`, infer it from the filename extension (for example `.pdf`, `.docx`, `.xlsx`, `.png`); an unknown extension keeps `application/octet-stream` - `inst-intake-mime`
12. [x] - `p1` - **IF** the MIME type is `text/csv` - `inst-intake-csv`
    1. [x] - `p1` - Accept it as `text/plain` when `rag.allow_csv_upload` is true (default), otherwise treat it as unsupported - `inst-intake-csv-remap`
13. [x] - `p1` - **IF** the MIME type is not in the allowlist (PDF, DOCX, PPTX, XLSX, plain text, Markdown, HTML, JSON, source code in Python, Java, JavaScript, TypeScript, Rust, Go, C#, Ruby and SQL, and the images `image/png`, `image/jpeg`, `image/webp`, `image/gif`) - `inst-intake-unsupported`
    1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[content_type].reason` `UNSUPPORTED_CONTENT_TYPE` - `inst-intake-return-unsupported`
14. [x] - `p1` - Classify the kind: `image` for the four image MIME types, `document` for every other supported type - `inst-intake-kind`
15. [x] - `p1` - **IF** the kind is `image` and the `disable_images` kill switch of the policy snapshot is on - `inst-intake-images-disabled`
    1. [x] - `p1` - **RETURN** 400 `failed_precondition` with `violations[{subject: images, type: FEATURE_DISABLED}]`; the upload performs no vision capability check - `inst-intake-return-images-disabled`
16. [x] - `p1` - Run the purpose routing algorithm and **RETURN** its Problem when it rejects - `inst-intake-purposes`
17. [x] - `p1` - Compute the effective per-file byte limit as the minimum of the kind-specific gear limit (`rag.uploaded_file_max_size_kb` default 25600 for documents, `rag.uploaded_image_max_size_kb` default 5120 for images, times 1024) and the model's `general_config.max_file_size_mb` times 1048576 when set - `inst-intake-limit`
18. [x] - `p1` - **RETURN** the intake result - `inst-intake-return`

### Purpose Routing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing`

**Input**: the MIME type, the kind, the kill switches and the chat model's `tool_support`

**Output**: the flags `for_file_search` and `for_code_interpreter`, or a Problem (DESIGN 4 `Code Interpreter Tool Availability`, `Code Interpreter Quota Enforcement (P1 Determinism Rules)` upload phase).

**Steps**:
1. [x] - `p1` - Derive the flags from the MIME type: XLSX (`application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`) gives `for_code_interpreter` true and `for_file_search` false; every other document type gives `for_file_search` true and `for_code_interpreter` false; images give both false - `inst-route-derive`
2. [x] - `p1` - **IF** `for_code_interpreter` is true and either the `disable_code_interpreter` kill switch is on or the chat model's `tool_support.code_interpreter` is false - `inst-route-ci-unavailable`
   1. [x] - `p1` - Set `for_code_interpreter` to false and keep the other purposes - `inst-route-ci-drop`
3. [x] - `p1` - **IF** the kind is `document` and no purpose remains - `inst-route-no-purpose`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[file].reason` `CODE_INTERPRETER_UNAVAILABLE` and `context.resource_type` `gts.cf.core.mini_chat.attachment.v1~` - `inst-route-return-ci-unavailable`
4. [x] - `p1` - Leave `for_file_search` unchanged by `disable_file_search` and by `tool_support.file_search`; both are stream-time gates of the file search contribution algorithm - `inst-route-file-search-untouched`
5. [x] - `p1` - **RETURN** the flags; a document with both flags true runs both purpose paths and reaches `ready` only when all of them succeed - `inst-route-return`

### Per-Chat Limits and Insert

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits`

**Input**: the scoped chat, the security context, the intake result, and (for the re-check) the final byte count of the uploaded file

**Output**: the inserted `pending` row, a 429 Problem, or (re-check) the failure `storage_limit_exceeded`. Rules: DESIGN 4 `RAG Quality & Scale Controls (P1)`, B.7, PRD `cpt-cf-mini-chat-fr-per-chat-doc-limits`.

**Steps**:
1. [x] - `p1` - Open one database transaction through the scoped persistence layer - `inst-lim-begin`
2. [x] - `p1` - **IF** the kind is `document` - `inst-lim-is-document`
   1. [x] - `p1` - DB: COUNT the `attachments` rows of the chat (`tenant_id`, `chat_id`) with `attachment_kind` `document`, `deleted_at IS NULL` and `status` other than `failed` (rows `pending` and `uploaded` count) - `inst-lim-count-docs`
   2. [x] - `p1` - **IF** the count is at least `rag.max_documents_per_chat` (default 50) - `inst-lim-doc-limit`
      1. [x] - `p1` - Roll back and **RETURN** 429 `resource_exhausted` with `violations[{subject: document_limit}]` - `inst-lim-return-doc-limit`
3. [x] - `p1` - DB: SUM `size_bytes` of the chat's `attachments` rows with `deleted_at IS NULL` and `status` other than `failed`, images included - `inst-lim-sum-size`
4. [x] - `p1` - **IF** the sum is at least `rag.max_total_upload_mb_per_chat` (default 100) times 1048576 - `inst-lim-storage-limit`
   1. [x] - `p1` - Roll back and **RETURN** 429 `resource_exhausted` with `violations[{subject: storage_limit}]` - `inst-lim-return-storage-limit`
5. [x] - `p1` - DB: INSERT the `attachments` row with a new UUID v4 `id`, `tenant_id` and `chat_id` from the scoped chat (never from input), `uploaded_by_user_id` the caller, the normalized `filename`, the resolved `content_type`, `size_bytes` 0, `storage_backend` from the intake, `status` `pending`, `attachment_kind`, `for_file_search`, `for_code_interpreter`, `secondary_status` `not_attempted`, `doc_summary`, `summary_model` and `summary_updated_at` NULL, `cleanup_status` NULL with `cleanup_attempts` 0, and `created_at` and `updated_at` from the application clock - `inst-lim-insert`
6. [x] - `p1` - Commit and increment `mini_chat_attachments_pending` - `inst-lim-commit`
7. [x] - `p1` - **IF** this is the re-check after the provider upload - `inst-lim-recheck`
   1. [x] - `p1` - DB: SUM `size_bytes` of the chat's other non-deleted, non-failed attachments; **IF** that sum plus the final byte count exceeds the storage limit, **RETURN** the failure `storage_limit_exceeded` - `inst-lim-recheck-sum`
8. [x] - `p1` - **RETURN** the inserted row (or the passed re-check) - `inst-lim-return`

### Provider File Upload

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload`

**Input**: the `pending` row, the intake result and the body stream of the `file` part

**Output**: the row in `status` `uploaded` with `provider_file_id` and `size_bytes`, or a failure kind (`file_too_large`, `upload_failed`, `storage_limit_exceeded`) with the provider file id when one exists. Rules: DESIGN 2.2 `Files API upload field mapping (P1)`, 3.6 `File Upload`, B.8 `Streaming upload`.

**Steps**:
1. [x] - `p1` - Build a multipart request with `purpose` `assistants` and a `file` part whose provider-side filename is `{chat_id}_{attachment_id}.{ext}` and whose content type is the resolved MIME type - `inst-pfu-build`
2. [x] - `p1` - Stream each body chunk into the provider request as it arrives and count the bytes; never buffer the whole body; for an image keep a copy of the bytes in memory only while the count stays at or below `thumbnail.max_decode_bytes` - `inst-pfu-stream`
3. [x] - `p1` - **IF** the byte count exceeds the effective per-file limit - `inst-pfu-too-large`
   1. [x] - `p1` - Drop the provider request at once and **RETURN** the failure `file_too_large` without a provider file id - `inst-pfu-return-too-large`
4. [x] - `p1` - API: `POST /{alias}{prefix}/files` through OAGW under the S2S context (OpenAI `/v1/files`, Azure `/openai/files` with `api-version`); the call is not retried and carries no idempotency key - `inst-pfu-post`
5. [x] - `p1` - **IF** the response is not successful or the transport fails - `inst-pfu-failed`
   1. [x] - `p1` - **RETURN** the failure `upload_failed`; a client retry is a new upload with a new attachment and a new provider file id - `inst-pfu-return-failed`
6. [x] - `p1` - DB: UPDATE the row with `provider_file_id` from the response, `size_bytes` equal to the byte count, `status` `uploaded` and `updated_at` now, by `id` and `tenant_id`; the row leaves `pending` here, so decrement `mini_chat_attachments_pending` in this step - `inst-pfu-set-uploaded`
7. [x] - `p1` - Run the storage re-check of the per-chat limits algorithm with the byte count and **RETURN** `storage_limit_exceeded` with the provider file id when it fails - `inst-pfu-recheck`
8. [x] - `p1` - Keep `provider_file_id` internal: it is never returned by any API, SSE payload, error text or metric label - `inst-pfu-internal-id`
9. [x] - `p1` - **RETURN** the provider file id and the byte count - `inst-pfu-return`

### Secondary Copy

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy`

**Input**: the `uploaded` image row, the kept image bytes (when any) and the chat model's provider entry

**Output**: the row's `secondary_*` columns. Rules: DESIGN 2.2 `File storage (P1)`, 3.2 `llm_provider`, ADR-0005. FEATURE 0004 reads `secondary_file_id` only when `secondary_status` is `uploaded`.

**Steps**:
1. [x] - `p1` - **IF** the chat model's provider kind is not `anthropic_messages`, the kind is `document`, or the image bytes were not kept because they exceeded `thumbnail.max_decode_bytes` - `inst-sec-skip`
   1. [x] - `p1` - **RETURN** with `secondary_status` left `not_attempted` - `inst-sec-return-skip`
2. [x] - `p1` - DB: UPDATE `secondary_status` to `pending` - `inst-sec-pending`
3. [x] - `p1` - API: upload the kept bytes through the Anthropic Files client over OAGW under the S2S context, on the Anthropic provider's upstream alias for the caller's tenant, with only the `file` part and no `purpose` - `inst-sec-upload`
4. [x] - `p1` - **IF** the upload succeeded - `inst-sec-ok`
   1. [x] - `p1` - DB: UPDATE `secondary_file_id` to the returned id, `secondary_status` to `uploaded` and `secondary_provider_kind` to `anthropic` - `inst-sec-set-uploaded`
5. [x] - `p1` - **ELSE** - `inst-sec-failed`
   1. [x] - `p1` - DB: UPDATE `secondary_status` to `failed`, log the cause without provider text in any client-visible place, and continue the image upload; the attachment still becomes `ready` and the Anthropic adapter cannot reference this image - `inst-sec-set-failed`
6. [x] - `p1` - **RETURN** the secondary outcome - `inst-sec-return`

### Vector Store Provisioning

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning`

**Input**: the scoped chat (`tenant_id`, `chat_id`), the storage provider and its `storage_backend` label

**Output**: the chat's provider `vector_store_id`, or the failure `vector_store_failed` or `provider_mismatch`. Rules: DESIGN 3.7 `Table: chat_vector_stores` (`Concurrency invariants`, `Tenant isolation invariants`, `Creation protocol (P1, attachment service)`, `Stale placeholder reclaim`), 4 `Vector Store Scope (P1)`. No database transaction or connection is held across a provider call; every query filters on `tenant_id` and `chat_id`, never on the row `id` or the `vector_store_id` alone.

**Steps**:
1. [x] - `p1` - DB: SELECT the `chat_vector_stores` row by `tenant_id` and `chat_id` of the scoped chat - `inst-vs-select`
2. [x] - `p1` - **IF** a row exists with a non-NULL `vector_store_id` - `inst-vs-fast-path`
   1. [x] - `p1` - **IF** its `provider` differs from the resolved `storage_backend`, **RETURN** the failure `provider_mismatch` - `inst-vs-mismatch`
   2. [x] - `p1` - **RETURN** the stored `vector_store_id` (reuse) - `inst-vs-return-reuse`
3. [x] - `p1` - **IF** a row exists with a NULL `vector_store_id` whose `created_at` is more than 120 s old and the protocol has not restarted yet in this request - `inst-vs-stale`
   1. [x] - `p1` - DB: DELETE that placeholder guarded by its `id`, `tenant_id`, `chat_id` and `vector_store_id IS NULL`, then **GO TO** step 1 once - `inst-vs-reclaim`
4. [x] - `p1` - **IF** a row exists with a NULL `vector_store_id` that is not stale - `inst-vs-in-progress`
   1. [x] - `p1` - **GO TO** step 8 (loser path) - `inst-vs-goto-loser`
5. [x] - `p1` - DB: INSERT the placeholder row (new `id`, `tenant_id`, `chat_id`, `vector_store_id` NULL, `provider` equal to `storage_backend`, `file_count` 0, `created_at` now) and commit it on its own - `inst-vs-insert`
6. [x] - `p1` - **IF** the insert violated `UNIQUE(tenant_id, chat_id)` - `inst-vs-lost`
   1. [x] - `p1` - **GO TO** step 8 (loser path); the loser never calls provider create - `inst-vs-goto-loser-race`
7. [x] - `p1` - Winner path - `inst-vs-winner`
   1. [x] - `p1` - API: `POST /{alias}{prefix}/vector_stores` through OAGW under the S2S context - `inst-vs-create`
   2. [x] - `p1` - **IF** the create failed, DB: DELETE the placeholder best effort and **RETURN** the failure `vector_store_failed` - `inst-vs-create-failed`
   3. [x] - `p1` - DB: UPDATE `vector_store_id` to the new id where the row `id` matches and `vector_store_id IS NULL` (compare-and-set) - `inst-vs-cas`
   4. [x] - `p1` - **IF** the update affected no row (the placeholder was reclaimed meanwhile), API: `DELETE /{alias}{prefix}/vector_stores/{new_id}` best effort and **GO TO** step 8 - `inst-vs-cas-lost`
   5. [x] - `p1` - **RETURN** the new `vector_store_id` - `inst-vs-return-created`
8. [x] - `p1` - Loser path: **FOR EACH** of at most 5 polls, waiting 200 ms before the first and doubling the wait before each next one - `inst-vs-loser-poll`
   1. [x] - `p1` - DB: SELECT the row by `tenant_id` and `chat_id`; **IF** its `vector_store_id` is set, apply the provider check of step 2 and **RETURN** it - `inst-vs-loser-read`
9. [x] - `p1` - **RETURN** the failure `vector_store_failed` (no store appeared within 5 polls) - `inst-vs-return-loser-failed`

### In-Request Indexing Wait

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait`

**Input**: the `vector_store_id`, the `provider_file_id`, the `attachment_id` and the request start instant

**Output**: `ready`, `still_in_progress` or the failure `indexing_failed`. Rules: DESIGN 3.6 `File Upload` (fixed, not configurable values), ADR-0007.

**Steps**:
1. [x] - `p1` - Set the deadline to the request start instant plus 25 s - `inst-iw-deadline`
2. [x] - `p1` - API: `POST /{alias}{prefix}/vector_stores/{vector_store_id}/files` with the provider file id and exactly one attribute, `attachment_id` equal to the attachment id; the call is not retried - `inst-iw-add`
3. [x] - `p1` - **IF** the add call failed - `inst-iw-add-failed`
   1. [x] - `p1` - **RETURN** the failure `indexing_failed` - `inst-iw-return-add-failed`
4. [x] - `p1` - Take the indexing status from the response; a response without `status` counts as `in_progress` - `inst-iw-initial-status`
5. [x] - `p1` - Set the next wait to 250 ms - `inst-iw-initial-wait`
6. [x] - `p1` - **WHILE** the status is `in_progress` - `inst-iw-loop`
   1. [x] - `p1` - **IF** the current time plus the next wait reaches the deadline, **RETURN** `still_in_progress` - `inst-iw-deadline-reached`
   2. [x] - `p1` - Wait the next wait, then double it up to at most 2 s - `inst-iw-backoff`
   3. [x] - `p1` - API: `GET /{alias}{prefix}/vector_stores/{vector_store_id}/files/{provider_file_id}` with a timeout that ends at the deadline - `inst-iw-read`
   4. [x] - `p1` - **IF** the read failed transiently (provider 5xx, gateway failure, or the read timed out), keep the status `in_progress` and remember the error - `inst-iw-transient`
   5. [x] - `p1` - **ELSE IF** the read failed otherwise, **RETURN** the failure `indexing_failed` - `inst-iw-read-failed`
   6. [x] - `p1` - **ELSE** take the status from the response, counting a missing `status` as `in_progress` - `inst-iw-status`
7. [x] - `p1` - **IF** the status is `completed` - `inst-iw-completed`
   1. [x] - `p1` - **RETURN** `ready`, so `file_search` finds the document as soon as the upload returns - `inst-iw-return-ready`
8. [x] - `p1` - **RETURN** the failure `indexing_failed` for `failed`, `cancelled` or any unknown status - `inst-iw-return-failed`

### Background Indexing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing`

**Input**: `tenant_id`, `chat_id`, `attachment_id`, `vector_store_id`, `provider_file_id`, the storage provider and the gear cancellation token

**Output**: the row in `ready`, or in `failed` with `indexing_failed` and an enqueued cleanup message, or unchanged. The task is spawned after the 201 response, is not persisted, runs under the S2S context for provider calls and filters every query on `tenant_id`. Rules: DESIGN 3.6 `File Upload`, B.9.5, ADR-0007; all values below are fixed.

**Steps**:
1. [x] - `p1` - Record the task start instant; the overall limit is 10 minutes after it - `inst-bg-start`
2. [x] - `p1` - **WHILE** no outcome is set - `inst-bg-loop`
   1. [x] - `p1` - **IF** the gear cancellation token fired, stop without changes; the row stays `uploaded` and the upload reaper later marks it `upload_abandoned` - `inst-bg-gear-stop`
   2. [x] - `p1` - **IF** 10 minutes passed since the start, set the outcome `timeout` - `inst-bg-timeout`
   3. [x] - `p1` - DB: UPDATE `updated_at` to now where `id` and `tenant_id` match, `status` is `uploaded`, `deleted_at IS NULL` and `cleanup_status IS NULL` (round heartbeat, every 20 s) - `inst-bg-heartbeat`
   4. [x] - `p1` - **IF** the heartbeat affected no row (the attachment was deleted, its chat was deleted, or it is no longer `uploaded`), stop without changes; such a row never becomes `ready` - `inst-bg-row-gone`
   5. [x] - `p1` - Poll for one round of 20 s: wait 250 ms before the first read and double the wait up to 5 s; read `GET /{alias}{prefix}/vector_stores/{vector_store_id}/files/{provider_file_id}`; a missing `status` and a transient read error keep polling (the first transient error is logged at `warn`, the last one is remembered) - `inst-bg-round`
   6. [x] - `p1` - **IF** a read returned `completed`, set the outcome `ready`; **IF** it returned `failed`, `cancelled` or an unknown status, or the read failed non-transiently, set the outcome `failed` - `inst-bg-outcome`
3. [x] - `p1` - **MATCH** the outcome - `inst-bg-match`
   - [x] - `p1` - **CASE** `ready`: DB: UPDATE `status` to `ready` and `updated_at` to now guarded by `status` `uploaded`, `deleted_at IS NULL` and `cleanup_status IS NULL`; on a database error make 4 attempts in total, 1, 2 and 4 s apart; on success record `mini_chat_attachment_background_indexing{result="ready"}`; when all attempts fail leave the row `uploaded` and record `result="set_ready_failed"` - `inst-bg-case-ready`
   - [x] - `p1` - **CASE** `failed` or `timeout`: in one transaction UPDATE `status` to `failed`, `error_code` to `indexing_failed`, `cleanup_status` to `pending`, `cleanup_updated_at` and `updated_at` to now with the same guard, and enqueue an attachment cleanup message with `event_type` `attachment_indexing_failed` through the cleanup enqueue algorithm; commit, fire the outbox wake after the commit and record `result="failed"` or `result="timeout"`; a timeout log names the last transient read error - `inst-bg-case-failed`
4. [x] - `p1` - **IF** the guarded update affected no row, roll back and enqueue nothing - `inst-bg-guard-lost`
5. [x] - `p1` - **IF** the failure transaction itself fails, leave the row `uploaded` so the upload reaper finishes it - `inst-bg-tx-failed`
6. [x] - `p1` - Never delete the provider file inline from this task; the attachment cleanup handler deletes it with retries - `inst-bg-no-inline-delete`
7. [x] - `p1` - Keep the heartbeat interval (20 s) at most half the minimum `upload_reaper.stale_after_secs` (60 s), checked at compile time - `inst-bg-heartbeat-bound`
8. [x] - `p1` - **RETURN** - `inst-bg-return`

### Image Thumbnail

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail`

**Input**: the kept image bytes (when any), the image MIME type and the `thumbnail` configuration

**Output**: the WebP thumbnail bytes with width and height, or skipped. Rules: DESIGN 3.6 `File Upload` (`Thumbnail generation details`, `Thumbnail storage invariant`), B.8. A skip never fails the upload and never sets `error_code`.

**Steps**:
1. [x] - `p1` - **IF** the image bytes were not kept (the upload is larger than `thumbnail.max_decode_bytes`, default 33554432) - `inst-th-not-kept`
   1. [x] - `p1` - **RETURN** skipped - `inst-th-return-not-kept`
2. [x] - `p1` - Read the header dimensions; **IF** width times height exceeds `thumbnail.max_pixels` (default 100000000) or width times height times 4 exceeds `thumbnail.max_decode_bytes`, **RETURN** skipped - `inst-th-prescreen`
3. [x] - `p1` - Decode with the decoder allocation capped at `thumbnail.max_decode_bytes`; **IF** decoding fails for any reason (malformed data, unsupported sub-format, allocation cap), **RETURN** skipped - `inst-th-decode`
4. [x] - `p1` - Resize to fit inside `thumbnail.width` by `thumbnail.height` (default 128 by 128) keeping the aspect ratio, without cropping - `inst-th-resize`
5. [x] - `p1` - Encode as `image/webp`; **IF** the encoded bytes exceed `thumbnail.max_bytes` (default 131072), **RETURN** skipped without a retry at lower quality - `inst-th-encode`
6. [x] - `p1` - Store the thumbnail only in the `attachments` row; never upload it to a provider or any object storage - `inst-th-storage`
7. [x] - `p1` - **RETURN** the bytes, width and height - `inst-th-return`

### Upload Failure Handling

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure`

**Input**: the inserted row, the failure kind and the provider file id (when one exists)

**Output**: the row in `failed` with its `error_code`, the permit released, and the Problem returned to the client. Rules: DESIGN 3.3 `Upload Attachment` error table, 3.6 `File Upload`, 3.7 `Table: attachments` (`error_code` values).

**Steps**:
1. [x] - `p1` - DB: UPDATE the row to `status` `failed`, `error_code` equal to the failure kind (`file_too_large`, `upload_failed`, `storage_limit_exceeded`, `vector_store_failed`, `indexing_failed`; a provider mismatch is stored as `vector_store_failed`) and `updated_at` now, guarded by `status` `pending` or `uploaded`; when the row was `pending` before this update (`file_too_large`, `upload_failed`), it leaves `pending` here, so decrement `mini_chat_attachments_pending` in the same step, and a row that already left `pending` at `inst-pfu-set-uploaded` is not decremented again - `inst-uf-mark-failed`
2. [x] - `p1` - **IF** a provider file id exists - `inst-uf-has-file`
   1. [x] - `p1` - API: `DELETE /{alias}{prefix}/files/{provider_file_id}` best effort, fire and forget, not retried; the outcome is only logged - `inst-uf-delete-file`
   2. [x] - `p1` - **IF** a secondary copy is `uploaded`, delete it through the Anthropic Files client the same way - `inst-uf-delete-secondary`
3. [x] - `p1` - Run the upload request completion algorithm with the outcome `error`, which releases the concurrency permit and records `mini_chat_attachment_upload{kind, result="error"}`; the in-request failure handling covers every failure of the provider file upload step and of the file search path (flow rule `inst-doc-failure-rule`) and of the image flow provider upload - `inst-uf-metrics`
4. [x] - `p1` - **MATCH** the failure kind - `inst-uf-match`
   - [x] - `p1` - **CASE** `file_too_large`: **RETURN** 400 `out_of_range` with `field_violations[content_length].reason` `FILE_TOO_LARGE` - `inst-uf-case-too-large`
   - [x] - `p1` - **CASE** `storage_limit_exceeded`: **RETURN** 429 `resource_exhausted` with `violations[{subject: storage_limit}]` - `inst-uf-case-storage`
   - [x] - `p1` - **CASE** `provider_mismatch`: **RETURN** 409 `already_exists` with `resource_name` `provider_mismatch` - `inst-uf-case-mismatch`
   - [x] - `p1` - **CASE** `upload_failed`, `vector_store_failed` or `indexing_failed`: **RETURN** 503 `service_unavailable` with `Retry-After: 10`, `context.retry_after_seconds` 10 and the generic detail `Service temporarily unavailable`; the body carries neither the `error_code` nor provider text - `inst-uf-case-unavailable`
5. [x] - `p1` - Keep the row visible through the get flow with `status` `failed` and its `error_code`; a failed row counts toward neither per-chat limit - `inst-uf-visible`

### Upload Request Completion

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion`

**Input**: the upload request (whether it holds a concurrency permit, its resolved MIME type or kind, the byte count when known) and its outcome: a 201 response (`ready` or `uploaded`) or a Problem

**Output**: the concurrency permit released exactly once and one `mini_chat_attachment_upload` sample for the request. Called on every return path of the document and image upload flows and of the upload intake algorithm after the permit was taken, so the per-process `rag.max_concurrent_uploads` permits cannot leak (DESIGN B.7, `Metric series` of DESIGN 4).

**Steps**:
1. [x] - `p1` - **IF** the request holds no permit (it returned before the permit was taken: admission refusal, missing model, unresolvable storage provider, busy limit) - `inst-uc-no-permit`
   1. [x] - `p1` - **RETURN** without releasing anything and without recording an upload metric - `inst-uc-return-no-permit`
2. [x] - `p1` - Release the permit exactly once; a second call for the same request does nothing, and a request dropped by a client disconnect or a gear stop releases it as well, because the permit lives as long as the request and not only until its return statement - `inst-uc-release`
3. [x] - `p1` - Derive the metric `kind`: `image` when the resolved MIME type is one of the four image types, otherwise `document` (also for a failure before the kind was classified) - `inst-uc-kind`
4. [x] - `p1` - Record `mini_chat_attachment_upload{kind, result}` once: `result` `ok` for a 201 response, `ready` and `uploaded` alike, and `error` for every Problem - `inst-uc-metric`
5. [x] - `p1` - **IF** the outcome is a 201 response, record `mini_chat_attachment_upload_bytes{kind}` with the byte count - `inst-uc-bytes`
6. [x] - `p1` - Leave the pending gauge to the transitions that move the row out of `pending` (`inst-pfu-set-uploaded`, `inst-uf-mark-failed`, `inst-rp-commit`); this algorithm never changes it - `inst-uc-no-gauge`
7. [x] - `p1` - **RETURN** - `inst-uc-return`

### Attachment Detail Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection`

**Input**: an `attachments` row

**Output**: the `AttachmentDetail` JSON object (DESIGN 3.3 `Get Attachment`; OpenAPI `AttachmentDetailDto`, `ImgThumbnailDto`).

**Steps**:
1. [x] - `p1` - Always include `id`, `filename`, `content_type`, `size_bytes`, `status` (`pending`, `uploaded`, `ready` or `failed`), `kind` (`document` or `image`) and `created_at` - `inst-proj-required`
2. [x] - `p1` - Include `error_code` only when `status` is `failed` - `inst-proj-error-code`
3. [x] - `p1` - Include `img_thumbnail` only when `status` is `ready`, `kind` is `image` and thumbnail bytes are stored, as an object with `content_type` `image/webp`, `width`, `height` and `data_base64` - `inst-proj-thumbnail`
4. [x] - `p1` - Omit `doc_summary` and `summary_updated_at`, which are always null (ADR-0007), and omit every other null optional field - `inst-proj-omit-null`
5. [x] - `p1` - Never include `provider_file_id`, `secondary_file_id`, `storage_backend`, the purpose flags, the cleanup columns, `uploaded_by_user_id` or any vector store identifier - `inst-proj-no-internal`
6. [x] - `p1` - **RETURN** the object - `inst-proj-return`

### Attachment Delete Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction`

**Input**: the scoped chat, the security context and the `attachment_id`

**Output**: deleted, already deleted, not found, locked, or a failure. Rules: DESIGN 4 `Attachment Mutability and Deletion`, `Attachment Deletion` (Phase 1), 3.6 `Cleanup responsibility boundaries`.

**Steps**:
1. [x] - `p1` - DB: SELECT the `attachments` row by `id`, `tenant_id` and `chat_id` of the scoped chat, including a soft-deleted row - `inst-dtx-select`
2. [x] - `p1` - **IF** no row exists or `uploaded_by_user_id` differs from the caller (checked before the idempotency check) - `inst-dtx-not-found`
   1. [x] - `p1` - **RETURN** not found - `inst-dtx-return-not-found`
3. [x] - `p1` - **IF** `deleted_at` is set - `inst-dtx-already`
   1. [x] - `p1` - **RETURN** already deleted; no outbox message is enqueued - `inst-dtx-return-already`
4. [x] - `p1` - Open one database transaction - `inst-dtx-begin`
5. [x] - `p1` - DB: check for any `message_attachments` row with this `attachment_id` and `chat_id` - `inst-dtx-lock-check`
6. [x] - `p1` - **IF** a row exists (any submitted message references the attachment) - `inst-dtx-locked`
   1. [x] - `p1` - Roll back and **RETURN** locked; the message's `attachments` array is never modified - `inst-dtx-return-locked`
7. [x] - `p1` - DB: UPDATE the row setting `deleted_at` and `updated_at` to now, `cleanup_status` to `pending` and `cleanup_updated_at` to now when `cleanup_status` is NULL (a set value keeps its state), guarded by `deleted_at IS NULL` - `inst-dtx-soft-delete`
8. [x] - `p1` - **IF** the update affected no row (a concurrent delete committed first) - `inst-dtx-race`
   1. [x] - `p1` - Roll back and **RETURN** already deleted - `inst-dtx-return-race`
9. [x] - `p1` - Enqueue one attachment cleanup message with `event_type` `attachment_deleted` through the cleanup enqueue algorithm in this transaction - `inst-dtx-enqueue`
10. [x] - `p1` - **IF** the payload exceeds the outbox size limit - `inst-dtx-too-large`
    1. [x] - `p1` - Roll back (the attachment stays active) and **RETURN** a failure mapped to 500 `internal` - `inst-dtx-return-too-large`
11. [x] - `p1` - **TRY** - `inst-dtx-try`
    1. [x] - `p1` - Commit, then fire the outbox wake - `inst-dtx-commit`
12. [x] - `p1` - **CATCH** a database failure - `inst-dtx-catch`
    1. [x] - `p1` - Roll back and **RETURN** a failure mapped to 500 `internal` - `inst-dtx-return-db-failure`
13. [x] - `p1` - From the commit on, exclude the attachment from chat metadata, from the `file_search` inclusion check (`status` `ready` and `deleted_at IS NULL` only) and from citations; make no provider call and no vector store call inside the request - `inst-dtx-exclusion`
14. [x] - `p1` - **RETURN** deleted - `inst-dtx-return-deleted`

### Attachment Cleanup Enqueue

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue`

**Input**: the attachment row, the `event_type` (`attachment_deleted`, `attachment_upload_abandoned` or `attachment_indexing_failed`) and the caller's open transaction

**Output**: one durable message on the queue `outbox.cleanup_queue_name` (default `mini-chat.attachment_cleanup`), committed or rolled back with the caller's transaction. Payload: DESIGN 4 `Attachment Deletion` Phase 1 table.

**Steps**:
1. [x] - `p1` - Serialize `event_type`, `tenant_id`, `chat_id`, `attachment_id`, `provider_file_id` (or null when the upload never reached the provider), `vector_store_id` null, `storage_backend`, `attachment_kind` and `deleted_at` equal to the enqueue time - `inst-enq-payload`
2. [x] - `p1` - **IF** the event is `attachment_deleted` and `secondary_status` is `uploaded` - `inst-enq-secondary`
   1. [x] - `p1` - Set `secondary_ref` to `file_id` (the `secondary_file_id`), `provider_kind` `anthropic` and the Anthropic `upstream_alias` resolved for the tenant; otherwise set `secondary_ref` to null - `inst-enq-secondary-ref`
3. [x] - `p1` - **IF** the event is `attachment_upload_abandoned` and a secondary copy exists - `inst-enq-reaper-secondary`
   1. [x] - `p1` - Keep `secondary_ref` null and log a warning with the `secondary_file_id`; that copy is not deleted (DESIGN B.9.5 limitation) - `inst-enq-reaper-warn`
4. [x] - `p1` - Enqueue through the foundation's transactional enqueuer inside the caller's transaction, partitioned by `tenant_id`; any `dedupe_key` follows the foundation prefix rule - `inst-enq-transactional`
5. [x] - `p1` - **IF** the serialized payload exceeds the outbox payload size limit - `inst-enq-too-large`
   1. [x] - `p1` - **RETURN** the enqueue error to the caller - `inst-enq-return-error`
6. [x] - `p1` - **RETURN** the enqueued message - `inst-enq-return`

### Attachment Cleanup Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler`

**Input**: one leased message of the attachment cleanup queue

**Output**: `Ok`, `Retry` or `Reject(reason)` for the shared outbox, and the row's cleanup state. A delivery of a message whose row is `failed` is a dead-letter replay and makes at most one provider delete attempt (the replay branch `inst-ch-replay`); the handler never returns `Retry` for a replay, so no automatic loop exists and only a further operator replay starts another attempt. This handler replaces the placeholder `UnwiredHandler` on that queue (implementation note 7). Rules: DESIGN 3.6 `Attachment cleanup state machine`, `Cleanup responsibility boundaries`, `Additional invariants`, 4 `Attachment Deletion` (Phase 2), B.9.2. It runs outside a user scope and filters every query on `tenant_id`.

**Steps**:
1. [x] - `p1` - Deserialize the payload - `inst-ch-parse`
2. [x] - `p1` - **IF** the payload is malformed - `inst-ch-malformed`
   1. [x] - `p1` - **RETURN** `Reject` at once (dead letter) - `inst-ch-return-malformed`
3. [x] - `p1` - DB: SELECT the parent chat by `tenant_id` and `chat_id` without the deleted filter - `inst-ch-load-chat`
4. [x] - `p1` - **IF** the chat is soft-deleted or missing - `inst-ch-chat-deleted`
   1. [x] - `p1` - **RETURN** `Ok` without action; chat cleanup owns the provider files of a deleted chat - `inst-ch-return-chat-owned`
5. [x] - `p1` - DB: SELECT the attachment row by `tenant_id`, `chat_id` and `attachment_id` - `inst-ch-load-row`
6. [x] - `p1` - **IF** a database error occurred in steps 3 to 5 - `inst-ch-db-error`
   1. [x] - `p1` - **RETURN** `Retry` without incrementing `cleanup_attempts` - `inst-ch-return-db-retry`
7. [x] - `p1` - **IF** the row is missing or its `cleanup_status` is already `done` - `inst-ch-terminal`
   1. [x] - `p1` - **RETURN** `Ok` without action (redelivery or dead-letter replay of a finished message) - `inst-ch-return-terminal`
8. [x] - `p1` - **IF** `provider_file_id` in the payload is null - `inst-ch-no-file`
   1. [x] - `p1` - Mark cleanup done as in `inst-ch-mark-done` and **RETURN** `Ok` - `inst-ch-return-no-file`
9. [x] - `p1` - **IF** the row's `cleanup_status` is `failed` (a dead-letter replay, or a redelivery after the exhausting delivery crashed before its `Reject`; the parent chat is not soft-deleted, step 4) - `inst-ch-replay`
   1. [x] - `p1` - API: `DELETE /{alias}{prefix}/files/{provider_file_id}` once, in the same way as `inst-ch-delete-file`; an unresolvable `storage_backend` counts as a failed attempt - `inst-ch-replay-delete`
   2. [x] - `p1` - **IF** the delete returned 2xx or 404 - `inst-ch-replay-ok`
      1. [x] - `p1` - **IF** `secondary_ref` is set, delete the secondary file best effort as in `inst-ch-delete-secondary`; its failure does not block the next step - `inst-ch-replay-secondary`
      2. [x] - `p1` - DB: UPDATE `cleanup_status` to `done` and `cleanup_updated_at` to now, guarded by `cleanup_status` `failed` (the guarded `failed` to `done` transition of the cleanup state machine), record `mini_chat_cleanup_completed{resource_type="file"}` and **RETURN** `Ok`; on a database error **RETURN** `Retry` without changing `cleanup_attempts`, and the redelivery repeats the idempotent delete - `inst-ch-replay-done`
   3. [x] - `p1` - **ELSE** DB: UPDATE `last_cleanup_error` to the cause and `cleanup_updated_at` to now, guarded by `cleanup_status` `failed`, keep `cleanup_status` `failed` and `cleanup_attempts` unchanged (it never grows beyond `cleanup_worker.max_attempts`), record neither `mini_chat_cleanup_failed` nor `mini_chat_cleanup_retry` again and **RETURN** `Reject` (dead letter) - `inst-ch-replay-failed`
10. [x] - `p1` - API: `DELETE /{alias}{prefix}/files/{provider_file_id}` on the storage provider that the storage dispatcher maps from `storage_backend`, through OAGW under the S2S context; an unresolvable `storage_backend` counts as a failed attempt - `inst-ch-delete-file`
11. [x] - `p1` - **IF** the delete returned a status other than 2xx or 404, or the transport failed - `inst-ch-delete-failed`
    1. [x] - `p1` - DB: UPDATE `cleanup_attempts` to its value plus 1, `last_cleanup_error` to the cause, `cleanup_updated_at` to now, guarded by `cleanup_status` `pending` - `inst-ch-record-attempt`
    2. [x] - `p1` - **IF** `cleanup_attempts` reached `cleanup_worker.max_attempts` (default 5), DB: UPDATE `cleanup_status` to `failed` with the same guard, record `mini_chat_cleanup_failed{resource_type="file"}` and **RETURN** `Reject` (dead letter); the row is terminal unresolved provider-file debt for automatic processing, and only the replay branch (`inst-ch-replay`) of a later operator replay of this dead-lettered message can clear it - `inst-ch-exhausted`
    3. [x] - `p1` - Record `mini_chat_cleanup_retry{resource_type="file", reason="provider_error"}` and **RETURN** `Retry`, leaving `cleanup_status` `pending`; the shared outbox applies lease-aware backoff capped at 30 s - `inst-ch-return-retry`
12. [x] - `p1` - **IF** `secondary_ref` is set - `inst-ch-secondary`
    1. [x] - `p1` - API: delete the secondary file through the Anthropic Files client on `secondary_ref.upstream_alias`; a failure is logged and does not block the next step; when no Anthropic Files client exists, record `mini_chat_secondary_cleanup_skipped{provider_kind}` - `inst-ch-delete-secondary`
13. [x] - `p1` - DB: UPDATE `cleanup_status` to `done` and `cleanup_updated_at` to now, guarded by `cleanup_status` `pending`, and record `mini_chat_cleanup_completed{resource_type="file"}` - `inst-ch-mark-done`
14. [x] - `p1` - Make no Vector Stores API call; the document leaves the chat vector store only when its provider file is deleted, and the store itself is deleted by chat cleanup - `inst-ch-no-vector-store`
15. [x] - `p1` - **RETURN** `Ok` - `inst-ch-return-ok`

### Upload Reaper

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper`

**Input**: the `upload_reaper` configuration, the leader elector with the Lease role `mini-chat-upload-reaper`, the application clock and the gear cancellation token

**Output**: abandoned rows moved to `failed` with `upload_abandoned`, with cleanup messages for rows that have a provider file. Rules: DESIGN 3.2 `upload reaper`, B.9.5, ADR-0007. The foundation spawns the task after the outbox pipeline starts (`cpt-cf-mini-chat-algo-gear-foundation-gear-start`); this algorithm owns its behavior.

**Steps**:
1. [x] - `p1` - **IF** `upload_reaper.enabled` is false - `inst-rp-disabled`
   1. [x] - `p1` - **RETURN** without scanning - `inst-rp-return-disabled`
2. [x] - `p1` - **WHILE** the gear cancellation token has not fired, run one scan every `upload_reaper.scan_interval_secs` (default 60, range 1 to 3600) - `inst-rp-loop`
   1. [x] - `p1` - **IF** this instance does not hold the `mini-chat-upload-reaper` leadership, skip the scan - `inst-rp-leader`
   2. [x] - `p1` - Compute the cutoff as now minus `upload_reaper.stale_after_secs` (default 300, range 60 to 86400) on the application clock - `inst-rp-cutoff`
   3. [x] - `p1` - DB: SELECT at most 100 rows (fixed) of `attachments` with `status` `pending` or `uploaded`, `deleted_at IS NULL`, `cleanup_status IS NULL` and `updated_at` before the cutoff, oldest `updated_at` first, using `idx_attachments_stale_upload`; the comparison and order use the chronological timestamp key on SQLite, never raw text comparison - `inst-rp-select`
   4. [x] - `p1` - **FOR EACH** selected row - `inst-rp-each`
      1. [x] - `p1` - Open one transaction and DB: UPDATE the row to `status` `failed`, `error_code` `upload_abandoned` and `updated_at` now, guarded by the same status, `deleted_at IS NULL`, `cleanup_status IS NULL` and `updated_at` before the cutoff - `inst-rp-cas`
      2. [x] - `p1` - **IF** the update affected no row (the upload finished, or the row was deleted or claimed by chat cleanup), roll back and skip the row - `inst-rp-skip`
      3. [x] - `p1` - **IF** the row has a `provider_file_id`, DB: UPDATE `cleanup_status` to `pending` and `cleanup_updated_at` to now and enqueue an attachment cleanup message with `event_type` `attachment_upload_abandoned` in the same transaction - `inst-rp-enqueue`
      4. [x] - `p1` - Commit, fire the outbox wake, then record `mini_chat_attachment_upload_abandoned{from_status}` with the previous status and, when the previous status was `pending`, decrement `mini_chat_attachments_pending` because the row left `pending` - `inst-rp-commit`
      5. [x] - `p1` - **IF** the transaction failed, log it and continue with the next row; a later scan retries it - `inst-rp-row-failed`
   5. [x] - `p1` - Record `mini_chat_upload_reaper_scan_duration_seconds` - `inst-rp-duration`
3. [x] - `p1` - Leave the reaped row not soft-deleted and visible through the get flow; a `pending` row has no provider file id, so a file the provider stored before the request was dropped is not deleted (provider-side orphan, P2 reconciliation) - `inst-rp-limits`
4. [x] - `p1` - **RETURN** when the gear stops - `inst-rp-return`

### Ready Document Fact

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`

**Input**: the scoped chat (`tenant_id` and `chat_id` from the owner-scoped chat load, never from request input)

**Output**: the number of ready file-search documents of the chat; the fact "the chat has a ready document" is that number being at least 1. This feature owns the predicate: the file search contribution, the send preflight read of `cpt-cf-mini-chat-feature-message-streaming` and the quota estimate of `cpt-cf-mini-chat-feature-quota-and-billing` use no other definition. Rules: DESIGN 4 `File Search Tool Availability`, 5.5.6.

**Steps**:
1. [x] - `p1` - DB: COUNT the `attachments` rows of the chat under the tenant scope (`tenant_id` and `chat_id`, through the scoped persistence layer, so a foreign tenant or foreign chat row never counts) with `status` `ready`, `deleted_at IS NULL` and `for_file_search` true - `inst-rdf-count`
2. [x] - `p1` - Never use `attachment_kind` as the predicate: an XLSX file (kind `document`, `for_file_search` false, `for_code_interpreter` true) and an image (both flags false) are not ready documents, because neither is in the chat vector store - `inst-rdf-not-kind`
3. [x] - `p1` - Do not read `chat_vector_stores` here; a `ready` row with `for_file_search` implies a bound store, and the file search contribution checks the bound store separately (`inst-fs-vector-store`) - `inst-rdf-no-store`
4. [x] - `p1` - Serve every consumer from this predicate: the file search contribution algorithm, the send preflight read `inst-pre-read-attachments` of `cpt-cf-mini-chat-algo-message-streaming-send-preflight`, and the quota estimate facts (`inst-admit-preflight` of `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission` and `inst-est-surcharges` of `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation`); the surcharge and the tool therefore appear together or not at all - `inst-rdf-consumers`
5. [x] - `p1` - **RETURN** the count - `inst-rdf-return`

### Ready Code Interpreter Fact

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact`

**Input**: the scoped chat (`tenant_id` and `chat_id` from the owner-scoped chat load, never from request input)

**Output**: the ready code-interpreter attachments of the chat as a list ordered by `created_at` then `id`, with their `provider_file_id` values; the fact "the chat has a ready code-interpreter attachment" is the list being non-empty. This feature owns the predicate: the code interpreter contribution, the send preflight read and the quota estimate and daily code interpreter check use no other definition. Rules: DESIGN 4 `Code Interpreter Tool Availability`.

**Steps**:
1. [x] - `p1` - DB: SELECT the `attachments` rows of the chat under the tenant scope (`tenant_id` and `chat_id` through the scoped persistence layer) with `status` `ready`, `deleted_at IS NULL`, `for_code_interpreter` true and a non-null `provider_file_id`, ordered by `created_at` then `id` - `inst-rci-select`
2. [x] - `p1` - Never use `attachment_kind` as the predicate; a document routed to `file_search` only is not a code-interpreter attachment - `inst-rci-not-kind`
3. [x] - `p1` - Serve every consumer from this predicate: the code interpreter contribution algorithm, the send preflight read `inst-pre-read-attachments` of `cpt-cf-mini-chat-algo-message-streaming-send-preflight`, and the quota estimate and daily check facts (`inst-admit-preflight` of `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission`, `inst-est-surcharges` of `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation` and `inst-toolq-ci-applies` of `cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check`); callers that need only the fact use the list length - `inst-rci-consumers`
4. [x] - `p1` - **RETURN** the ordered list - `inst-rci-return`

### File Search Contribution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution`

**Input**: the scoped chat, the effective model entry (after the downgrade cascade), the kill switches and the `context.file_search_guard` text

**Output**: the `file_search` tool definition, its guard and the citation file map, or no contribution. Plugs into `cpt-cf-mini-chat-algo-message-streaming-tool-assembly`. Rules: DESIGN 4 `File Search Tool Availability`, `File Search Retrieval Scope`, `Retrieval Invariants`, `RAG Context Assembly Rules`, `Citation File ID and Title Resolution`, `File Search Per-Turn Call Limit Enforcement (P1)`.

**Steps**:
1. [x] - `p1` - **IF** the `disable_file_search` kill switch is on or the effective model's `tool_support.file_search` is false - `inst-fs-gated`
   1. [x] - `p1` - **RETURN** no contribution - `inst-fs-return-gated`
2. [x] - `p1` - Run the ready document fact algorithm (`cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`), the same predicate that the send preflight uses for the file search surcharge - `inst-fs-ready-docs`
3. [x] - `p1` - **IF** the count is 0 - `inst-fs-no-docs`
   1. [x] - `p1` - **RETURN** no contribution; `file_search` is never sent before the first document is `ready` - `inst-fs-return-no-docs`
4. [x] - `p1` - DB: SELECT the chat's `chat_vector_stores` row by `tenant_id` and `chat_id` with a non-NULL `vector_store_id`; **IF** none exists, log a warning and **RETURN** no contribution - `inst-fs-vector-store`
5. [x] - `p1` - Build the definition with `type` `file_search`, `vector_store_ids` holding only the chat's store and `max_num_results` from the effective model's catalog entry, with no attribute filter (retrieval covers every document of the chat store; `attachment_ids` never scopes it) - `inst-fs-definition`
6. [x] - `p1` - Attach the guard `context.file_search_guard` - `inst-fs-guard`
7. [x] - `p1` - DB: SELECT the chat's `ready`, non-deleted attachments with a non-null `provider_file_id` through the `(tenant_id, chat_id)` index and build the in-memory citation file map from `provider_file_id` to `attachment_id` and `filename`, before the provider call - `inst-fs-citation-map`
8. [x] - `p1` - Add no per-turn `file_search` call limit; calls are bounded only by the catalog `max_tool_calls` that the OpenAI Responses adapter sends, and chunks per call by `max_num_results` - `inst-fs-no-limit`
9. [x] - `p1` - **RETURN** the definition, the guard and the map; its presence turns knowledge search off for the turn - `inst-fs-return`

### Code Interpreter Contribution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution`

**Input**: the scoped chat, the effective model entry and the kill switches

**Output**: the `code_interpreter` tool definition and its `include` entry, or no contribution. Plugs into `cpt-cf-mini-chat-algo-message-streaming-tool-assembly`. Rules: DESIGN 4 `Code Interpreter Tool Availability`, `Code Interpreter Configuration`, B.6.1.

**Steps**:
1. [x] - `p1` - **IF** the `disable_code_interpreter` kill switch is on or the effective model's `tool_support.code_interpreter` is false - `inst-ci-gated`
   1. [x] - `p1` - **RETURN** no contribution, silently (no error, no guard); the daily code interpreter check is then skipped by the quota preflight - `inst-ci-return-gated`
2. [x] - `p1` - Run the ready code interpreter fact algorithm (`cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact`) and take its ordered `provider_file_id` list, the same predicate that the send preflight uses for the code interpreter surcharge and the daily check - `inst-ci-files`
3. [x] - `p1` - **IF** the list is empty - `inst-ci-none`
   1. [x] - `p1` - **RETURN** no contribution - `inst-ci-return-none`
4. [x] - `p1` - Build the definition with `type` `code_interpreter` and `container` `{type: auto, file_ids}` holding the listed ids, and the `include` entry `code_interpreter_call.outputs` - `inst-ci-definition`
5. [x] - `p1` - **RETURN** the definition and the `include` entry; there is no request flag for code interpreter - `inst-ci-return`

### Knowledge Search Contribution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution`

**Input**: the `knowledge_search` configuration, whether the file search contribution applied for the turn, the provider registry and the caller's tenant

**Output**: the `search_knowledge` tool definition with its guard, or no contribution. Rules: DESIGN 4 `Knowledge Search`, B.7 (knowledge search table).

**Steps**:
1. [x] - `p1` - **IF** `knowledge_search.enabled` is false (default) - `inst-ks-disabled`
   1. [x] - `p1` - **RETURN** no contribution - `inst-ks-return-disabled`
2. [x] - `p1` - **IF** the file search contribution applied for this turn - `inst-ks-exclusive`
   1. [x] - `p1` - **RETURN** no contribution; `file_search` wins and the two tools are never sent in one request - `inst-ks-return-exclusive`
3. [x] - `p1` - **IF** the provider entry named by `knowledge_search.provider_id` is not of kind `openai_responses` or `anthropic_messages`, has no upstream alias for the tenant, has an empty `api_version`, or the knowledge retriever is not configured - `inst-ks-unusable`
   1. [x] - `p1` - Log a warning and **RETURN** no contribution (neither tool nor guard) - `inst-ks-return-unusable`
4. [x] - `p1` - Build the tool named `search_knowledge` whose parameters are `query` (required text) and `top_k` (optional integer) - `inst-ks-definition`
5. [x] - `p1` - Attach the guard `knowledge_search.guard`; add no reserve surcharge - `inst-ks-guard`
6. [x] - `p1` - **RETURN** the definition, the guard and the tool-result format selected by the provider kind - `inst-ks-return`

### Knowledge Search Loop

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop`

**Input**: a provider iteration that ended with a tool-use outcome, the turn's accumulated provider input, the turn's cancellation token, `knowledge_search.max_calls_per_message` (default 3), `knowledge_search.top_k` (default 5) and `knowledge_search.max_chunk_chars` (default 2000)

**Output**: the next provider iteration, or a terminal outcome handed to `cpt-cf-mini-chat-algo-message-streaming-finalization`. Rules: DESIGN 4 `Knowledge Search`; billing of earlier iterations is not implemented (ADR-0008).

**Steps**:
1. [x] - `p1` - Count provider iterations of the turn; the hard cap is `knowledge_search.max_calls_per_message` plus 2 - `inst-kl-iterations`
2. [x] - `p1` - **WHILE** the last iteration ended with a tool-use outcome - `inst-kl-loop`
   1. [x] - `p1` - **IF** the tool name is not `search_knowledge`, hand the outcome `failed` with `unexpected_tool_use` to the finalization and stop - `inst-kl-unexpected`
   2. [x] - `p1` - **IF** the retrievals of this message already reached `max_calls_per_message`, answer the call with a short "search limit reached" output so the model answers from what it has - `inst-kl-limit-output`
   3. [x] - `p1` - **ELSE** increment the in-memory file search count, then API: `POST /{alias}/openai/vector_stores/{knowledge_search.vector_store_id}/search` with `api-version` through OAGW under the S2S context, with the model's `query` and `top_k` capped at `knowledge_search.top_k` - `inst-kl-retrieve`
   4. [x] - `p1` - **IF** the retrieval succeeded, trim each chunk to `max_chunk_chars`, answer the call with the chunks in the tool-result format of the provider kind, DB: increment `chat_turns.file_search_completed_count` guarded by `state` `running`, and record `mini_chat_knowledge_search{result="ok"}`, `mini_chat_knowledge_search_latency_ms` and `mini_chat_knowledge_search_chunks` - `inst-kl-success`
   5. [x] - `p1` - **ELSE** answer the call with a short failure output, record `mini_chat_knowledge_search{result="error"}` and do not increment `file_search_completed_count` - `inst-kl-failure`
   6. [x] - `p1` - Append the call and its output to the provider input - `inst-kl-append`
   7. [x] - `p1` - **IF** the next iteration would exceed the hard cap, hand the outcome `failed` with `agentic_iterations_exceeded` and the usage of the last iteration when known to the finalization, which ends the stream with SSE `error{code: "agentic_iterations_exceeded"}`, and stop - `inst-kl-cap`
   8. [x] - `p1` - **IF** the turn's cancellation token fired, stop; the disconnect path of `cpt-cf-mini-chat-feature-message-streaming` finalizes the turn - `inst-kl-cancelled`
   9. [x] - `p1` - Issue the next provider request through `cpt-cf-mini-chat-algo-message-streaming-provider-call` with the same tools and relay its events - `inst-kl-next`
3. [x] - `p1` - Settle only the final iteration's usage; the in-memory file search count is reported as `file_search_calls` in the usage and audit events - `inst-kl-settle`
4. [x] - `p1` - **RETURN** the final provider outcome to the finalization - `inst-kl-return`

### Attachment Error Contract

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract`

**Input**: an attachment-route failure

**Output**: the canonical Problem built by `cpt-cf-mini-chat-algo-gear-foundation-error-mapping` (fields `type`, `title`, `status`, `detail`, `instance`, `trace_id`, `context`; no top-level `code`). Normative: DESIGN 3.3 `Upload Attachment` and the REST error mapping, ADR-0004.

**Steps**:
1. [x] - `p1` - **MATCH** the failure - `inst-err-match`
   - [x] - `p1` - **CASE** missing, soft-deleted, foreign-user or foreign-tenant chat: 404 `not_found`, `context.resource_type` `gts.cf.core.mini_chat.chat.v1~` (never 403) - `inst-err-chat-404`
   - [x] - `p1` - **CASE** unknown, soft-deleted, other-chat or other-uploader attachment on get or delete: 404 `not_found`, `context.resource_type` `gts.cf.core.mini_chat.attachment.v1~` - `inst-err-attachment-404`
   - [x] - `p1` - **CASE** non-UUID path parameter: 400 `invalid_argument`, `invalid_path_params` - `inst-err-path`
   - [x] - `p1` - **CASE** chat model no longer in the catalog: 400 `invalid_argument`, `field_violations[model].reason` `INVALID_MODEL` - `inst-err-invalid-model`
   - [x] - `p1` - **CASE** multipart shape: 400 `invalid_argument` with `BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE` or `MISSING_CONTENT_TYPE` - `inst-err-multipart`
   - [x] - `p1` - **CASE** unsupported MIME type: 400 `invalid_argument`, `field_violations[content_type].reason` `UNSUPPORTED_CONTENT_TYPE` - `inst-err-unsupported`
   - [x] - `p1` - **CASE** code-interpreter-only file while code interpreter is unavailable: 400 `invalid_argument`, `field_violations[file].reason` `CODE_INTERPRETER_UNAVAILABLE`, attachment `resource_type` - `inst-err-ci`
   - [x] - `p1` - **CASE** image while `disable_images` is on: 400 `failed_precondition`, `violations[{subject: images, type: FEATURE_DISABLED}]` - `inst-err-images-disabled`
   - [x] - `p1` - **CASE** file above the effective per-file limit: 400 `out_of_range`, `field_violations[content_length].reason` `FILE_TOO_LARGE` (a body above the api-gateway body limit gets 413 from the gateway) - `inst-err-too-large`
   - [x] - `p1` - **CASE** per-chat document count or total size: 429 `resource_exhausted`, `violations[{subject: document_limit}]` or `violations[{subject: storage_limit}]` - `inst-err-limits`
   - [x] - `p1` - **CASE** vector store of another storage backend: 409 `already_exists`, `resource_name` `provider_mismatch` - `inst-err-mismatch`
   - [x] - `p1` - **CASE** delete of a referenced attachment: 409 `already_exists`, `resource_name` `attachment_locked` - `inst-err-locked`
   - [x] - `p1` - **CASE** provider Files or vector store failure, indexing failure within the deadline: 503 `service_unavailable`, `Retry-After: 10`, `context.retry_after_seconds` 10 - `inst-err-storage-503`
   - [x] - `p1` - **CASE** upload concurrency limit: 503 `service_unavailable`, `Retry-After: 5`, `context.retry_after_seconds` 5 - `inst-err-busy-503`
   - [x] - `p1` - **CASE** policy or storage provider resolution failure before the body is read, oversized delete payload, database error: 500 `internal` with a generic detail - `inst-err-internal`
2. [x] - `p1` - Never put a provider identifier, provider error text, `error_code` of the row or a credential into `detail`, `context` or a log line that reaches a client - `inst-err-no-provider-text`
3. [x] - `p1` - **RETURN** the Problem - `inst-err-return`

### Attachment Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics`

**Input**: the outcomes of the upload (through the upload request completion algorithm), background indexing, reaper, cleanup handler and knowledge search algorithms

**Output**: the series of DESIGN 4 `Metrics (OTLP) and Alerts (P1)` in the areas Upload reaper, Attachments, Knowledge search, Uploads and Cleanup; the instruments are registered by `cpt-cf-mini-chat-algo-gear-foundation-observability`.

**Steps**:
1. [x] - `p1` - Record `mini_chat_attachment_upload{kind, result}` once per upload request that took a permit, through the upload request completion algorithm on every return path: `result` `ok` for each 201 response (`uploaded` included, as DESIGN 4 states for `mini_chat_attachment_background_indexing`) and `error` for every Problem after the permit was taken, and `mini_chat_attachment_upload_bytes{kind}` with the byte count of a 201 upload - `inst-met-upload`
2. [x] - `p1` - Increment `mini_chat_attachments_pending` when a row is inserted as `pending` (`inst-lim-commit`) and decrement it exactly where the row leaves `pending`: when the provider upload stores the file id and moves it to `uploaded` (`inst-pfu-set-uploaded`), when the upload failure algorithm fails a row that was still `pending` (`inst-uf-mark-failed`), and when the reaper fails a `pending` row (`inst-rp-commit`); never in the flow tail - `inst-met-pending`
3. [x] - `p1` - Record `mini_chat_attachment_background_indexing{result}` with `ready`, `failed`, `timeout` or `set_ready_failed` once per background task that reaches an outcome - `inst-met-background`
4. [x] - `p1` - Record `mini_chat_attachment_upload_abandoned{from_status}` after each reaper commit and `mini_chat_upload_reaper_scan_duration_seconds` per scan - `inst-met-reaper`
5. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type="file"}`, `mini_chat_cleanup_retry{resource_type="file", reason="provider_error"}`, `mini_chat_cleanup_failed{resource_type="file"}` and `mini_chat_secondary_cleanup_skipped{provider_kind}` from the attachment cleanup handler - `inst-met-cleanup`
6. [x] - `p1` - Record `mini_chat_knowledge_search{result}`, `mini_chat_knowledge_search_latency_ms` and `mini_chat_knowledge_search_chunks` from the knowledge search loop - `inst-met-knowledge`
7. [x] - `p1` - Leave the declared-but-not-recorded series of this area unrecorded (`file_search_latency_ms`, `retrieval_latency_ms`, `retrieval_chunks_returned`, `retrieval_zero_hit`, `indexed_chunks_per_chat`, `upload_rejected`, `vector_stores_per_user`, `attachment_index`, `attachment_summary`, `attachments_failed`, `attachment_index_latency_ms`, `media_rejected`, `cleanup_backlog`) - `inst-met-deferred`
8. [x] - `p1` - Use only low-cardinality labels; never a tenant, user, chat, attachment, file or vector store identifier - `inst-met-labels`
9. [x] - `p1` - **RETURN** - `inst-met-return`

### Attachment Configuration

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-and-file-search-configuration`

**Input**: the configuration validated by `cpt-cf-mini-chat-algo-gear-foundation-config-validation`, the policy snapshot and the catalog entries

**Output**: the values this feature reads; this feature adds no configuration key.

**Steps**:
1. [x] - `p1` - Read `rag.uploaded_file_max_size_kb` (25600), `rag.uploaded_image_max_size_kb` (5120), `rag.max_documents_per_chat` (50), `rag.max_total_upload_mb_per_chat` (100), `rag.allow_csv_upload` (true), `rag.max_concurrent_uploads` (10, range 1 to 256) - `inst-cfg-rag`
2. [x] - `p1` - Read `thumbnail.width` (128), `thumbnail.height` (128), `thumbnail.max_bytes` (131072), `thumbnail.max_pixels` (100000000), `thumbnail.max_decode_bytes` (33554432) - `inst-cfg-thumbnail`
3. [x] - `p1` - Read `upload_reaper.enabled` (true), `upload_reaper.scan_interval_secs` (60), `upload_reaper.stale_after_secs` (300), `cleanup_worker.max_attempts` (5) and `outbox.cleanup_queue_name` (`mini-chat.attachment_cleanup`); the other `cleanup_worker` keys have no effect (ADR-0010) - `inst-cfg-workers`
4. [x] - `p1` - Read `knowledge_search.enabled` (false), `vector_store_id`, `provider_id`, `max_calls_per_message` (3), `top_k` (5), `max_chunk_chars` (2000), `guard`, and `context.file_search_guard` - `inst-cfg-knowledge`
5. [x] - `p1` - Read per request the kill switches `disable_images`, `disable_file_search`, `disable_code_interpreter` and the catalog fields `general_config.max_file_size_mb`, `tool_support.file_search`, `tool_support.code_interpreter`, `max_num_results` and `max_tool_calls` - `inst-cfg-policy`
6. [x] - `p1` - Keep the fixed values out of configuration: 25 s indexing deadline, 250 ms doubling to 2 s in-request polling, 20 s background rounds with 250 ms doubling to 5 s, 10-minute background limit, 4 set-ready attempts 1, 2 and 4 s apart, 120 s stale placeholder, 5 loser polls, 100 rows per reaper scan - `inst-cfg-fixed`
7. [x] - `p1` - **RETURN** the values - `inst-cfg-return`

## 4. States (CDSL)

### Attachment Status State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status`

**States**: pending, uploaded, ready, failed

**Initial State**: pending

**Transitions**:
1. [x] - `p1` - **FROM** pending **TO** uploaded **WHEN** the provider Files API returned a provider file id and the row stored it - `inst-as-uploaded`
2. [x] - `p1` - **FROM** uploaded **TO** ready **WHEN** every purpose path succeeded within the request (indexing `completed`, code-interpreter-only file, or image with thumbnail step done) or the background indexing read `completed` - `inst-as-ready`
3. [x] - `p1` - **FROM** pending **TO** failed **WHEN** the byte limit was exceeded or the provider upload failed (`file_too_large`, `upload_failed`), or the reaper found the row stale (`upload_abandoned`) - `inst-as-pending-failed`
4. [x] - `p1` - **FROM** uploaded **TO** failed **WHEN** the storage re-check, the vector store or the indexing failed in the request (`storage_limit_exceeded`, `vector_store_failed`, `indexing_failed`), the background indexing failed or timed out (`indexing_failed`), or the reaper found the row stale (`upload_abandoned`) - `inst-as-uploaded-failed`
5. [x] - `p1` - **FROM** ready **TO** failed **WHEN** never in P1; `ready` and `failed` are terminal, and a soft-delete is orthogonal to the status - `inst-as-terminal`

### Attachment Cleanup Status State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`

**States**: none (NULL), pending, done, failed

**Initial State**: none (NULL)

**Transitions**:
1. [x] - `p1` - **FROM** none **TO** pending **WHEN** the attachment is deleted, its chat is soft-deleted (applied by `cpt-cf-mini-chat-feature-chat-management`), the reaper fails an abandoned row with a provider file id, or the background indexing fails or times out - `inst-cs-pending`
2. [x] - `p1` - **FROM** pending **TO** done **WHEN** the provider file delete returned 2xx or 404, or the row had no provider file id - `inst-cs-done`
3. [x] - `p1` - **FROM** pending **TO** failed **WHEN** `cleanup_attempts` reached `cleanup_worker.max_attempts` after failed provider deletes; the row is terminal unresolved provider-file debt for automatic processing (it is never re-enqueued) and does not count as cleanup complete for chat purge while it is `failed` - `inst-cs-failed`
4. [x] - `p1` - **FROM** pending **TO** pending **WHEN** a provider delete failed below the budget (`cleanup_attempts` plus 1, `last_cleanup_error`, `cleanup_updated_at`) or an infrastructure error occurred (no increment) - `inst-cs-retry`
5. [x] - `p1` - **FROM** failed **TO** done **WHEN** an operator replay of the dead-lettered cleanup message re-attempted the provider file delete once and it returned 2xx or 404; the update is guarded by `cleanup_status` `failed`, and this is the only transition out of `failed` - `inst-cs-replay-done`
6. [x] - `p1` - **FROM** failed **TO** failed **WHEN** an operator replay re-attempted the provider delete and it failed (`last_cleanup_error` and `cleanup_updated_at` refreshed, `cleanup_attempts` unchanged); the handler rejects the message again, so no loop exists - `inst-cs-replay-failed`

### Secondary Copy State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy`

**States**: not_attempted, pending, uploaded, failed

**Initial State**: not_attempted

**Transitions**:
1. [x] - `p1` - **FROM** not_attempted **TO** pending **WHEN** an image of a chat served by an `anthropic_messages` provider was kept in memory and its secondary upload starts - `inst-sc-pending`
2. [x] - `p1` - **FROM** pending **TO** uploaded **WHEN** the Anthropic Files API returned a file id (stored with `secondary_provider_kind` `anthropic`) - `inst-sc-uploaded`
3. [x] - `p1` - **FROM** pending **TO** failed **WHEN** the secondary upload failed; the attachment itself is not failed - `inst-sc-failed`

### Chat Vector Store State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`

**States**: absent, placeholder, bound

**Initial State**: absent

**Transitions**:
1. [x] - `p1` - **FROM** absent **TO** placeholder **WHEN** the first document upload of the chat inserted the row with a NULL `vector_store_id` - `inst-vsm-placeholder`
2. [x] - `p1` - **FROM** placeholder **TO** bound **WHEN** the winner's compare-and-set stored the provider `vector_store_id` - `inst-vsm-bound`
3. [x] - `p1` - **FROM** placeholder **TO** absent **WHEN** the provider create failed and the winner deleted the placeholder, or a later upload reclaimed a placeholder older than 120 s - `inst-vsm-reclaimed`
4. [x] - `p1` - **FROM** bound **TO** absent **WHEN** chat cleanup of `cpt-cf-mini-chat-feature-background-maintenance` deleted the provider store (or got 404) after every attachment cleanup row of the chat became terminal; never by this feature - `inst-vsm-chat-cleanup`

## 5. Definitions of Done

### Upload Endpoint, Intake and Purpose Routing

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-intake`

The system **MUST** serve `POST /v1/chats/{id}/attachments` with admission (`upload_attachment`), chat model and storage provider resolution and the concurrency permit before any body byte is read, the multipart, MIME (allowlist, extension inference, CSV remap), filename, kind, `disable_images` and code-interpreter routing checks of the intake and purpose routing algorithms, and the effective per-file limit `min(gear kind limit, model max_file_size_mb)`.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `POST /v1/chats/{id}/attachments`
- DB: `cpt-cf-mini-chat-dbtable-chats`
- Entities: `Attachment`

### Per-Chat Limits and Provider File Upload

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload`

The system **MUST** enforce `rag.max_documents_per_chat` and `rag.max_total_upload_mb_per_chat` over non-deleted, non-failed rows (429 `document_limit` / `storage_limit`, re-checked with the final size), insert the `pending` row with server-derived ownership, stream the bytes to the storage provider through OAGW under the S2S context with `purpose` `assistants` and a byte counter that aborts with `FILE_TOO_LARGE`, store `provider_file_id` and `size_bytes` with `status` `uploaded`, never expose the provider file id, and on every return path of the upload flows (limit, intake after the permit, in-request failure, 201 `uploaded`, 201 `ready`) release the concurrency permit exactly once and record `mini_chat_attachment_upload` once, with `result` `ok` for each 201.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`, `cpt-cf-mini-chat-constraint-openai-compatible`

**Touches**:
- API: `POST /v1/chats/{id}/attachments`; OAGW `POST {prefix}/files`, `DELETE {prefix}/files/*`
- DB: `cpt-cf-mini-chat-dbtable-attachments`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `Attachment`

### Per-Chat Vector Store

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-vector-store`

The system **MUST** create at most one provider vector store per chat through the placeholder insert, single-winner create and compare-and-set protocol with the 5-poll loser path, 120 s stale reclaim and `provider_mismatch` rejection, access `chat_vector_stores` only by `(tenant_id, chat_id)`, never reuse a store across chats or tenants, and add each document with the single attribute `attachment_id`.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning`
- `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: OAGW `POST {prefix}/vector_stores*`, `DELETE {prefix}/vector_stores/*`
- DB: `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- DB Table: `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- Entities: `ChatVectorStore`

### Indexing Lifecycle With Deadline and Background Continuation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-indexing-lifecycle`

The system **MUST** wait for indexing within the request (250 ms doubling to 2 s, until 25 s after the request start; transient read errors keep polling; a missing status is `in_progress`), return 201 `ready` on `completed`, mark the row `failed` with `indexing_failed`, delete the provider file best effort and return 503 `Retry-After: 10` on failure, and on a still-running index return 201 `uploaded` and continue in a non-persisted background task (20 s rounds with heartbeat, 250 ms doubling to 5 s, 10-minute limit, guarded `ready` write with 4 attempts, one-transaction `failed` plus `attachment_indexing_failed` enqueue, stop without changes on deletion, chat deletion or gear stop).

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing`
- `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: OAGW `POST {prefix}/vector_stores/{id}/files`, `GET {prefix}/vector_stores/{id}/files/{file_id}`
- DB: `cpt-cf-mini-chat-dbtable-attachments`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `Attachment`, attachment cleanup outbox message

### Images, Thumbnails and the Anthropic Secondary Copy

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-images`

The system **MUST** upload images synchronously to the RAG provider without vector store indexing, generate a WebP thumbnail within the size, pixel and decode safeguards (skipping without failing the upload), store it only in the database, and for chats served by an `anthropic_messages` provider produce the secondary Anthropic copy whose `secondary_file_id` FEATURE 0004 uses only when `secondary_status` is `uploaded`.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-secondary-copy`
- `cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy`

**Constraints**: `cpt-cf-mini-chat-constraint-model-image-capability` (the vision check stays a send-time guard; the upload never checks it)

**Touches**:
- API: `POST /v1/chats/{id}/attachments`
- DB: `cpt-cf-mini-chat-dbtable-attachments`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `Attachment`

### Get Attachment API

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-get-api`

The system **MUST** serve `GET /v1/chats/{id}/attachments/{attachment_id}` with the `AttachmentDetail` projection (optional fields omitted when null, `error_code` only for `failed`, `img_thumbnail` only for a `ready` image, no summary fields, no internal or provider field) and 404 with the attachment type for unknown, soft-deleted, other-chat and other-uploader attachments.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `GET /v1/chats/{id}/attachments/{attachment_id}`
- DB: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `Attachment`

### Delete Attachment and Mutability

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-delete-api`

The system **MUST** serve `DELETE /v1/chats/{id}/attachments/{attachment_id}`: 404 for unknown, other-chat or other-uploader attachments (uploader check before the idempotency check), 204 without a new message for an already deleted one, 409 `attachment_locked` when any message references it, otherwise soft-delete with `cleanup_status` `pending` and one `attachment_deleted` message in one transaction, 204 after commit, and 500 when the payload is too large.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `DELETE /v1/chats/{id}/attachments/{attachment_id}`
- DB: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-attachments`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `Attachment`, attachment cleanup outbox message

### Attachment Cleanup Handler and Cleanup State Machine

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler`

The system **MUST** replace the placeholder handler of `mini-chat.attachment_cleanup` with the attachment cleanup handler: `Reject` for a malformed payload, `Ok` without action for a soft-deleted or missing parent chat, a missing row or a `done` row, `done` for a null provider file id, provider delete through OAGW with 2xx and 404 as success, `cleanup_attempts` plus `Retry` below `cleanup_worker.max_attempts` (default 5) and `failed` plus `Reject` at the limit, `Retry` without increment on database errors, best-effort secondary deletion, no Vector Stores API call, and for a `failed` row (dead-letter replay) exactly one provider delete re-attempt that sets `done` through the guarded `failed` to `done` transition on 2xx or 404 and otherwise keeps `failed` and returns `Reject` without growing `cleanup_attempts`, so a replay never loops.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler`
- `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: OAGW `DELETE {prefix}/files/*`
- DB: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chats`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: attachment cleanup outbox message

### Upload Reaper Recovery

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-reaper`

The system **MUST** run the upload reaper on the leader only (Lease `mini-chat-upload-reaper`), select at most 100 stale `pending` or `uploaded` rows without `cleanup_status` older than `upload_reaper.stale_after_secs`, fail each with `upload_abandoned` through a guarded update, set `cleanup_status` `pending` and enqueue `attachment_upload_abandoned` in the same transaction when a provider file id exists, and record the reaper metrics after commit.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: none
- DB: `cpt-cf-mini-chat-dbtable-attachments`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Entities: `Attachment`, attachment cleanup outbox message

### Tool Contributions, Retrieval Scope and Citation File Map

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-tool-contributions`

The system **MUST** register the file search, code interpreter and knowledge search contributions with the tool assembly of FEATURE 0004: `file_search` over the whole chat vector store with `max_num_results` and the guard only when a `ready` document exists, the store is bound, the effective model supports it and `disable_file_search` is off; `code_interpreter` with the ready file ids, `container.type` `auto` and `include` `code_interpreter_call.outputs` only when the effective model supports it and `disable_code_interpreter` is off; `search_knowledge` only when enabled, usable and `file_search` is absent; the citation file map before the provider call; and the two shared fact algorithms that the send preflight and the quota estimate use (an XLSX-only chat reserves no `file_search` surcharge and sends no `file_search` tool).

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`, `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: `POST /v1/chats/{id}/messages:stream` (contributions only)
- DB: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- Entities: `Attachment`, `ChatVectorStore`

### Knowledge Search Agentic Loop

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-knowledge-loop`

The system **MUST** answer `search_knowledge` calls through the knowledge retriever over OAGW with `top_k` capped and chunks trimmed, run at most `knowledge_search.max_calls_per_message` retrievals per message (later calls get a limit output), cap provider iterations at that value plus 2 with `agentic_iterations_exceeded`, end a call of any other tool name with `unexpected_tool_use`, count successful retrievals in `file_search_completed_count`, and settle only the final iteration.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: OAGW `POST /openai/vector_stores/{id}/search`
- DB: `cpt-cf-mini-chat-dbtable-chat-turns`
- Entities: `Turn` (counter only)

### Error Contract, Observability and Configuration

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`

The system **MUST** map every attachment-route failure to the canonical Problem of the error contract algorithm without provider text, record the attachment metric series with low-cardinality labels, keep the declared-but-unrecorded series unrecorded, and read only the configuration keys and fixed values of the configuration algorithm.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-configuration`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- API: every route of this feature
- DB: none
- Entities: none

### Deferred and Not-Implemented Scope Stays Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-deferred-scope`

The system **MUST NOT** implement the document summary (`doc_summary`, `summary_model` and `summary_updated_at` stay NULL; no background task; no context tier), the maximum indexed chunks per chat, `retrieval_k` or `max_retrieved_tokens_per_turn` settings, a dedicated per-turn `file_search` call limit or `file_search_calls_exceeded` code, a per-user daily `file_search` limit or `quota_usage.file_search_calls` counting, immediate exclusion of a deleted document from `file_search` results, attachment-scoped retrieval, document search for Anthropic chats, a daily image quota or per-message image byte cap, a provider-file reconciliation job, hard-purge of soft-deleted attachments, or deletion of the Anthropic copy of a reaped image, and **MUST** keep them recorded as not implemented (ADR-0007, ADR-0008, ADR-0009).

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution`
- `cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection`

**Constraints**: `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: none
- DB: none
- Entities: none

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-and-file-search-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Acceptance criteria mapping** (acceptance-criteria.md section `Attachments`):

| Acceptance criterion | Flows, algorithms and states | Definitions of done | Test layer |
|---|---|---|---|
| Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation | upload-document, upload-image, get-attachment, delete-attachment flows; upload-intake, purpose-routing, per-chat-limits, provider-file-upload, upload-failure, upload-completion, detail-projection, delete-transaction, error-contract algorithms; attachment-status state | upload-intake, limits-and-provider-upload, images, get-api, delete-api | Unit (intake, routing, limit arithmetic, projection, error mapping, permit release and metric recording on every return path, including 201 `uploaded`, the 429 limits and intake Problems after the permit); repository over SQLite (limit counting with failed and deleted rows, delete transaction and lock, enqueue in the same transaction); in-process REST with a mock OAGW (each Problem, 201, 200, 204, 404, 409, no provider id in any body) |
| Asynchronous indexing lifecycle, including provider failure and timeout handling | upload-document flow; vector-store-provisioning, indexing-wait, background-indexing algorithms; attachment-status and chat-vector-store states | vector-store, indexing-lifecycle | Unit with paused time (backoff schedule 250 ms to 2 s and to 5 s, the 25 s deadline, the 10-minute limit, status mapping, transient versus other read errors); repository over SQLite (placeholder race, CAS, stale reclaim, guarded heartbeat and `ready` writes, the failure transaction with its message); in-process REST with a mock OAGW scripting `completed`, `failed`, `cancelled`, missing status, 5xx and `in_progress` past 25 s |
| Attachments are correctly made available to the relevant provider tools | ask-about-files flow; ready-document-fact, ready-code-interpreter-fact, file-search, code-interpreter and knowledge contributions; knowledge-loop; secondary-copy algorithm | tool-contributions, knowledge-loop, images | Unit (each inclusion rule, the two fact predicates with XLSX-only, image-only and mixed chats, kill switch and `tool_support` gate, mutual exclusion, citation map, loop caps); in-process SSE over SQLite with a mock OAGW recording the proxied request (`tools` entries with `vector_store_ids`, `max_num_results`, `container.file_ids`, `include`, `input_image` `file_id`, the Anthropic secondary id, file citations with `attachment_id` and filename, deleted attachments omitted) |
| Cleanup and abandoned-upload recovery behave correctly under failure | delete-attachment flow; cleanup-enqueue, cleanup-handler, upload-reaper, background-indexing algorithms; cleanup-status state | cleanup-handler, upload-reaper, delete-api | Handler tests over SQLite with a mock OAGW (2xx, 404, 500 until the fifth attempt gives `failed` and `Reject`, malformed payload, soft-deleted chat, null provider file id, `done` row redelivery, `failed` row replay with provider success (`done`) and provider failure (stays `failed`, `Reject`, no attempt growth), database error without increment, secondary failure not blocking); reaper tests with a controllable clock (cutoff, 100-row cap, CAS skip, enqueue only with a provider file id, non-leader scans nothing, SQLite fractional-second ordering) |

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic and timing with paused time: MIME resolution, CSV remap, filename normalization, purpose routing, the effective size limit, limit arithmetic, the indexing and background polling schedules and outcome mapping, the thumbnail safeguards (oversize, pixel bomb header, decode cap, encoded size), the projection, the error mapping, the cleanup payload, the tool contributions and the knowledge loop.
- Repository tests with SQLite cover every guarded update of this feature, the shared fact queries, the vector store protocol races, the delete transaction, the enqueue inside the domain transaction, and the reaper selection with timestamps whose fractional seconds differ in length.
- In-process REST and SSE tests drive the router with stubbed platform clients over SQLite and a mock OAGW `ServiceGatewayClientV1` that records each proxied request (URI `/{alias}{path}`, method, S2S security context, multipart parts or JSON body) and scripts the Files, Vector Stores, Anthropic Files, knowledge search and Responses streams; no network is used.
- No tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Reliability and data integrity**: every domain write that needs a cleanup message enqueues it in the same transaction and fires the wake after commit (implementation note 3); every status and cleanup transition is a guarded conditional update; the vector store protocol never creates two stores for a chat; deleting a row never relies on provider success. **Security**: admission, tenant and owner scoping and 404 masking come from the foundation; attachment get and delete also check the uploader; provider calls run under the gear's S2S context; no provider identifier, credential or provider error text reaches a client or a metric label; file content is never logged. **Observability**: the series of the attachment metrics algorithm. **Rollback**: a failed transaction leaves no partial row change or message; schema rollback is owned by the foundation migrations. **Compile-time gates**: those of the foundation (workspace lints, domain layering lint keeping `domain/` free of `crate::infra`, `sea_orm`, `axum` and `http`, the example server build), plus the compile-time check that the background heartbeat interval is at most half the minimum `stale_after_secs`.

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| The upload reaper behavior is specified and owned here although the foundation's gear start names `cpt-cf-mini-chat-feature-background-maintenance` for it | DECOMPOSITION 2.5 assigns the reaper to this feature; the foundation only spawns it | DECOMPOSITION 2.5, 2.7 |
| The unsupported MIME reason is `UNSUPPORTED_CONTENT_TYPE` on field `content_type`; the task brief called it `UNSUPPORTED_FILE_TYPE` | The DESIGN, PRD and ADR-0004 fix the reason name (the old 415 `unsupported_file_type` was replaced); the DESIGN does not name the field, `content_type` matches `MISSING_CONTENT_TYPE` | DESIGN 3.3, ADR-0004 |
| Admission and model resolution run before the multipart headers are parsed | The DESIGN sequence lists MIME resolution first, but its error table requires the chat 404 and `INVALID_MODEL` before the body is read; checking them first makes a foreign chat 404 regardless of the body | DESIGN 3.3, 3.6 |
| The row is inserted before the bytes are streamed; the per-file size is enforced by the byte counter during the provider upload and the storage limit is checked at insert and re-checked with the final size | The DESIGN streams without buffering, so the size is known only at the end, and lists `file_too_large` and `storage_limit_exceeded` as row `error_code` values | DESIGN 3.6, 3.7, B.8 |
| A row inserted with `size_bytes` 0 counts 0 toward the storage limit of a concurrent upload until its provider upload finishes; concurrent uploads to one chat are not serialized | No lock is specified; the re-check bounds the overshoot to concurrent in-flight uploads | DESIGN 4 `RAG Quality & Scale Controls (P1)` |
| Every in-request failure after the provider upload (storage re-check, vector store failure, provider mismatch, indexing failure) deletes the provider file best effort, not only `indexing_failed` | A `failed` row is neither reaped nor queued for cleanup, so otherwise its provider file has no cleanup path | DESIGN 3.6 |
| A provider mismatch stores `error_code` `vector_store_failed` on the row | The DESIGN lists no `provider_mismatch` row code; the mismatch is detected in the vector store step | DESIGN 3.7 `Table: chat_vector_stores` |
| The loser path waits 200 ms before the first poll and doubles the wait (5 polls, about 6.2 s) | The DESIGN fixes 5 polls with exponential backoff but no base value | DESIGN 3.7 `Creation protocol` |
| A failed Anthropic secondary upload sets `secondary_status` `failed` and does not fail the attachment | The DESIGN does not make the secondary copy a purpose path; the Anthropic adapter cannot use such an image, matching the cross-provider downgrade limitation | DESIGN 3.2, 4 `Known limitation — attachments after a cross-provider downgrade` |
| `disable_file_search` and `tool_support.file_search` do not change routing at upload; they gate the tool at stream time | The DESIGN defines upload-time filtering only for code interpreter | DESIGN 4 `Code Interpreter Tool Availability`, 3.2 `llm_provider` |
| The "ready document" fact (tool inclusion and the file search surcharge) is a `ready`, non-deleted attachment with `for_file_search` in the chat, and the "ready code-interpreter attachment" fact is a `ready`, non-deleted attachment with `for_code_interpreter`; this feature owns both predicates (`cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`, `cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact`) | An XLSX-only chat has no vector store, so `file_search` cannot be sent and its surcharge must not be reserved; the DESIGN defines the tool by the vector store and the purpose flags, not by `attachment_kind` | DESIGN 4 `File Search Tool Availability`, `Code Interpreter Tool Availability`, 5.5.6 |
| Cross-feature touchpoint: the implementation of this feature switches the existing send preflight read `inst-pre-read-attachments` of `cpt-cf-mini-chat-algo-message-streaming-send-preflight` (FEATURE 0004, code `chat_facts` in `infra/storage/turn_repo.rs`, which today counts `ready`, non-deleted rows with `attachment_kind` `document`, XLSX included) to the two predicates, and thereby the request facts that FEATURE 0003 consumes (`inst-admit-preflight` of `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission`, `inst-est-surcharges` of `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation`, `inst-toolq-ci-applies` of `cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check`; their "ready documents" and "ready XLSX attachments" wording means these two facts); owner of the change: the implementer of this feature, reviewed by the owners of FEATURE 0004 and FEATURE 0003 | FEATURE 0004 and 0003 are implemented against a document-kind count, so an XLSX-only chat today reserves a `file_search` surcharge for a tool that is never sent; the sibling documents keep their step wording and the predicates are defined once here | DESIGN 4 `File Search Tool Availability`, 5.5.6; FEATURE 0003, 0004 |
| Any `message_attachments` row locks the attachment, also when its message belongs to a soft-deleted turn | The DESIGN guard is "referenced by any submitted message" and links are never removed | DESIGN 4 `Attachment Mutability and Deletion` |
| The cleanup handler acks a missing parent chat like a soft-deleted one, acks a missing row or a `done` row, and counts an unresolvable `storage_backend` as a failed attempt | Makes redelivery and dead-letter replay idempotent and bounds an unusable backend by the retry budget | DESIGN 3.6 `Additional invariants` |
| The cleanup state machine gains the guarded transition `failed` to `done` (and `failed` to `failed` with `Reject`), taken only by a dead-letter replay that re-attempts the provider delete once; the DESIGN lists only `pending` to `done` and `pending` to `failed` and calls both terminal | DESIGN 3.6 says a rejected message "moves to the shared outbox dead-letter store for operator recovery" and invariant 3 says clearing per-file debt "requires operator intervention"; without this transition a replay would always be a no-op and there would be no recovery path. The replay is one attempt with `Reject` on failure and no attempt growth, so it is bounded, and a `failed` row stays unequal to cleanup complete for chat purge until it is `done`. The `failed` check covers abandoned uploads and failed indexing too, whose rows are `failed` but not soft-deleted | DESIGN 3.6 `Attachment cleanup state machine`, `Additional invariants`, B.9.2 |
| `mini_chat_attachment_upload` uses `result` `ok` (every 201, `uploaded` included) or `error` (every Problem after the permit was taken) and `kind` `image` only for an image MIME type, otherwise `document`; `mini_chat_attachments_pending` is incremented at insert and decremented where the row leaves `pending` (provider upload stored, upload failure, reaper), not in the flow tail | The DESIGN names the series and `result="ok"` only; recording on every return path keeps the permit and the metric consistent, and a request that fails before the kind is classified still needs a bounded `kind` label | DESIGN 4 `Metric series`, B.7 |
| The background indexing task does not hold an upload concurrency permit | The permit bounds in-request uploads; the background task is bounded by its 10-minute limit | DESIGN B.7 |
| The attachment cleanup queue handler replaces the foundation's placeholder `UnwiredHandler` | The outbox keeps the messages until the owning feature wires its handler | Implementation note 7 |
| Where `config/mini-chat.yaml` or the extracted OpenAPI and the DESIGN differ, the DESIGN governs | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`
- `cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`, `cpt-cf-mini-chat-constraint-quota-before-outbound`

**Touches**:
- API: every route of this feature
- DB: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`
- Entities: `Attachment`, `ChatVectorStore`, attachment cleanup outbox message

## 6. Acceptance Criteria

The first four items are the DECOMPOSITION 2.5 "Acceptance Criteria Covered" and the `Attachments` items of `acceptance-criteria.md`; each is both implemented and tested (test layers in the mapping table of `cpt-cf-mini-chat-dod-attachments-and-file-search-tests-and-deviations`). The remaining items pin the details behind them.

- [ ] Upload, get and delete work for documents and images with size, type and per-chat limit validation: a PDF upload returns 201 `ready` and an XLSX upload returns 201 `ready` without a vector store call; a PNG upload returns 201 `ready` with a WebP `img_thumbnail`; an unsupported type returns 400 `UNSUPPORTED_CONTENT_TYPE`, a file above `min(gear limit, model max_file_size_mb)` returns 400 `out_of_range` `FILE_TOO_LARGE` with the row `failed` (`file_too_large`), the 51st document returns 429 `document_limit`, an upload over 100 MiB of non-deleted, non-failed attachments returns 429 `storage_limit`; GET returns the detail without provider identifiers, with `error_code` only for `failed`; DELETE returns 204, then 204 again with no second outbox message, 409 `attachment_locked` for a referenced attachment, and 404 with the attachment type for another uploader's attachment (Unit, repository over SQLite, in-process REST with mock OAGW).
- [ ] The asynchronous indexing lifecycle handles provider failure and timeout: with the mock vector store answering `completed` after polling, the upload returns 201 `ready`; with `failed`, `cancelled`, an unknown status or a 4xx status read, the row becomes `failed` with `indexing_failed`, one provider file delete is attempted and the response is 503 with `Retry-After: 10` and no `error_code` in the body; with `in_progress` past 25 s, the response is 201 `uploaded`, the background task refreshes `updated_at` every round and then sets `ready` on `completed`, or sets `failed` with `indexing_failed`, `cleanup_status` `pending` and one `attachment_indexing_failed` message on failure or after 10 minutes; a deletion of the attachment or its chat stops the task without changes (Unit with paused time, repository, in-process REST).
- [ ] Attachments are made available to the relevant provider tools: in a chat with a `ready` document the proxied request carries `{type: file_search, vector_store_ids: [chat store], max_num_results}` and the `context.file_search_guard`, and none before the first `ready` document, with `disable_file_search` on or on a model without `tool_support.file_search`; in a chat whose only ready attachment is an XLSX the send preflight counts no ready document, reserves no `file_search` surcharge and the request carries no `file_search` tool; a `ready` XLSX gives `{type: code_interpreter, container: {type: auto, file_ids}}` and `include` `code_interpreter_call.outputs` unless `disable_code_interpreter` is on or the effective model lacks support; an image in `attachment_ids` appears as `input_image` with its internal file id (or its `uploaded` secondary id on Anthropic); with knowledge search enabled, `search_knowledge` is sent only when `file_search` is not; file citations carry `attachment_id` and the filename, and a deleted attachment is never cited (Unit, in-process SSE with mock OAGW).
- [ ] Cleanup and abandoned-upload recovery behave correctly under failure: the cleanup handler marks `done` on provider 2xx or 404 and on a null provider file id, records attempts and returns `Retry` on 500 until the fifth failed attempt sets `failed` and returns `Reject`, returns `Retry` without increment on a database error, `Reject` on a malformed payload and `Ok` without a provider call for a soft-deleted parent chat; replaying the dead-lettered message of a `failed` row makes one provider delete attempt and sets `done` on 2xx or 404, or keeps `failed` and returns `Reject` again on a provider error, and a replay of a `done` row is acknowledged without a provider call; the reaper on the leader fails `pending` and `uploaded` rows older than `stale_after_secs` with `upload_abandoned`, enqueues `attachment_upload_abandoned` only when a provider file id exists, skips rows with a `cleanup_status` or refreshed `updated_at`, handles at most 100 rows per scan and does nothing on a non-leader (handler and reaper tests over SQLite with mock OAGW and a controllable clock).
- [ ] The upload checks before the body is read: a foreign or deleted chat gets 404 with the chat type, a chat model missing from the catalog gets 400 `INVALID_MODEL`, the eleventh concurrent upload with the default limit gets 503 with `Retry-After: 5`, and the mock OAGW records no Files API request in each case.
- [ ] The upload concurrency permit and the upload metric follow every return path: after a 201 `uploaded` response (background indexing still running), a 429 `document_limit` or `storage_limit`, an intake Problem after the permit was taken (for example `MISSING_FILE` or `UNSUPPORTED_CONTENT_TYPE`), an in-request failure and a 201 `ready`, the permit is free again, so more than `rag.max_concurrent_uploads` sequential uploads with slow indexing are never answered 503; `mini_chat_attachment_upload` has one sample per request that took a permit, `result` `ok` for each 201 including `uploaded` and `error` otherwise, and `mini_chat_attachments_pending` returns to its prior value once the row left `pending` (provider upload stored, upload failure or reaper).
- [ ] Code interpreter routing at upload: with `disable_code_interpreter` on, or a chat model whose `tool_support.code_interpreter` is false, an XLSX upload returns 400 `CODE_INTERPRETER_UNAVAILABLE` with the attachment `resource_type` and no row.
- [ ] Images: with `disable_images` on an image upload returns 400 `failed_precondition` `FEATURE_DISABLED` subject `images`; an image on a model without `VISION_INPUT` uploads successfully; a pixel-bomb header, an image above `thumbnail.max_decode_bytes` or an encoded thumbnail above `thumbnail.max_bytes` gives `ready` with no `img_thumbnail` and no `error_code`; in an Anthropic chat the secondary copy is uploaded with no `purpose` field and `secondary_status` becomes `uploaded`, and a failed secondary upload leaves the attachment `ready` with `secondary_status` `failed`.
- [ ] The per-chat vector store is created once: two concurrent first document uploads produce one provider create call and both attachments use the same `vector_store_id`; a stale placeholder older than 120 s is reclaimed; a store created for another `storage_backend` gives 409 `provider_mismatch`; each document is added with only the `attachment_id` attribute.
- [ ] The provider file upload request uses `purpose` `assistants`, the provider-side filename `{chat_id}_{attachment_id}.{ext}`, the URI `/{alias}/v1/files` (or `/{alias}/openai/files` with `api-version`) and the gear's S2S security context, and carries no credential.
- [ ] Deleting an unreferenced document removes it at once from the message `attachments` arrays, from the `file_search` inclusion check and from citations, while the provider file is deleted only by the cleanup handler and no Vector Stores API call is made.
- [ ] The knowledge search loop: a `search_knowledge` call is answered with chunks trimmed to `max_chunk_chars` and `top_k` capped at `knowledge_search.top_k`, the fourth call with the default limit receives the limit output, the sixth provider iteration ends the turn with SSE `error{code: "agentic_iterations_exceeded"}`, a call of another tool name ends it with `unexpected_tool_use`, and successful retrievals increment `chat_turns.file_search_completed_count`.
- [ ] Metrics: `mini_chat_attachment_upload{kind, result}`, `mini_chat_attachment_upload_bytes`, `mini_chat_attachments_pending`, `mini_chat_attachment_background_indexing{result}`, `mini_chat_attachment_upload_abandoned{from_status}`, `mini_chat_upload_reaper_scan_duration_seconds`, the cleanup counters with `resource_type="file"` and the knowledge search series are recorded with no identifier in any label, and the deferred series stay unrecorded.
- [ ] The deferred scope stays unimplemented: `doc_summary` and `summary_updated_at` are never present, there is no chunk cap, no per-turn or daily `file_search` limit, no immediate retrieval exclusion, no attachment-scoped retrieval and no Anthropic document search.
- [ ] Unit tests live in the gear crate, repository, handler, reaper and REST/SSE tests run in-process over SQLite with a mock OAGW `ServiceGatewayClientV1` and no network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
