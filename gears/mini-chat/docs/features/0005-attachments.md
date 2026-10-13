---
description: "Attachments feature of the mini-chat gear: multipart upload of documents and images with MIME, size, kill-switch, capability and per-chat limit validation and bounded concurrency, provider file storage through OAGW with the secondary Anthropic copy, the race-safe per-chat vector store, synchronous and background indexing, WebP thumbnails, get and soft delete with the attachment cleanup outbox handler, the abandoned-upload reaper, and the ready-attachment and citation data the send pipeline consumes."
---

# Feature: Attachments

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-attachments-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-attachments`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Upload Attachment](#upload-attachment)
  - [Get Attachment](#get-attachment)
  - [Delete Attachment](#delete-attachment)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Upload Context Resolution](#upload-context-resolution)
  - [Multipart Reading](#multipart-reading)
  - [File Classification and Purpose Routing](#file-classification-and-purpose-routing)
  - [Bounded File Content Read](#bounded-file-content-read)
  - [Pending Row Insert and Per-Chat Limits](#pending-row-insert-and-per-chat-limits)
  - [Storage Request Construction](#storage-request-construction)
  - [Provider File Upload](#provider-file-upload)
  - [Secondary Anthropic Copy](#secondary-anthropic-copy)
  - [Chat Vector Store Resolution](#chat-vector-store-resolution)
  - [Document Indexing Within the Request](#document-indexing-within-the-request)
  - [Background Indexing Continuation](#background-indexing-continuation)
  - [Thumbnail Generation](#thumbnail-generation)
  - [Ready Transition](#ready-transition)
  - [Upload Failure Handling](#upload-failure-handling)
  - [Attachment Detail Projection](#attachment-detail-projection)
  - [Attachment Soft Delete](#attachment-soft-delete)
  - [Attachment Cleanup Payload](#attachment-cleanup-payload)
  - [Attachment Cleanup Handler](#attachment-cleanup-handler)
  - [Abandoned Upload Reaper](#abandoned-upload-reaper)
  - [Attachment Metrics Recording](#attachment-metrics-recording)
- [4. States (CDSL)](#4-states-cdsl)
  - [Attachment Status State Machine](#attachment-status-state-machine)
  - [Attachment Cleanup State Machine](#attachment-cleanup-state-machine)
  - [Secondary Copy State Machine](#secondary-copy-state-machine)
  - [Chat Vector Store State Machine](#chat-vector-store-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Upload Endpoint and Validation](#upload-endpoint-and-validation)
  - [Provider File Storage Through OAGW](#provider-file-storage-through-oagw)
  - [Chat Vector Store and Indexing](#chat-vector-store-and-indexing)
  - [Image Thumbnails and Secondary Copies](#image-thumbnails-and-secondary-copies)
  - [Get Attachment API](#get-attachment-api)
  - [Attachment Deletion](#attachment-deletion)
  - [Attachment Cleanup Outbox Handler](#attachment-cleanup-outbox-handler)
  - [Upload Reaper Worker](#upload-reaper-worker)
  - [Send Pipeline Readiness and Citation Data](#send-pipeline-readiness-and-citation-data)
  - [Attachment Error Contract](#attachment-error-contract)
  - [Attachment Metrics](#attachment-metrics)
  - [REST Registration and OpenAPI](#rest-registration-and-openapi)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature lets users ground a chat in their own files (DESIGN §3.3 "Upload Attachment", "Get Attachment", §3.6 `cpt-cf-mini-chat-seq-file-upload`, §4 "File Search Retrieval Scope", "Attachment Deletion", Appendix B.8 and B.9.5). It owns `POST /v1/chats/{id}/attachments`: multipart reading with the exact field and reason contract, MIME normalization against the allowlist, kind and purpose routing (`for_file_search`, `for_code_interpreter`), the `disable_images` and `disable_code_interpreter` kill switches, the chat model's code-interpreter capability and `max_file_size_mb`, the per-kind size limits, the per-chat document count and storage limits, and the per-process upload concurrency bound. It then owns provider storage through the OAGW proxy: the Files API upload with `purpose` `assistants`, the secondary copy of images in Anthropic chats, the lazily created per-chat vector store with its race-safe single row, document indexing with a synchronous wait inside the 25 s request deadline and a background continuation of up to 10 minutes, and WebP thumbnails for images. It owns `GET /v1/chats/{id}/attachments/{attachment_id}`, the soft delete with its 409 lock and idempotent repeat, the attachment cleanup outbox payload and the attachment cleanup handler that replaces the foundation placeholder, and the leader-elected abandoned-upload reaper. Finally it guarantees the ready-attachment state, the provider file ids and the chat vector store id that the send pipeline of `cpt-cf-mini-chat-feature-streaming-turns` reads, and the `provider_file_id` to attachment mapping its citations resolve against. `doc_summary` is never produced.

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | Validation, pending row and read API (no provider call) | `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`, `cpt-cf-mini-chat-algo-attachments-read-multipart`, `cpt-cf-mini-chat-algo-attachments-classify-file`, `cpt-cf-mini-chat-algo-attachments-read-file-content`, `cpt-cf-mini-chat-algo-attachments-insert-pending`, `cpt-cf-mini-chat-algo-attachments-project-detail`, `cpt-cf-mini-chat-flow-attachments-get-attachment` |
| 2 | Provider file storage and the code-interpreter path | `cpt-cf-mini-chat-algo-attachments-build-storage-request`, `cpt-cf-mini-chat-algo-attachments-upload-provider-file`, `cpt-cf-mini-chat-algo-attachments-mark-ready`, `cpt-cf-mini-chat-algo-attachments-fail-upload`, `cpt-cf-mini-chat-flow-attachments-upload-attachment` (XLSX path), `cpt-cf-mini-chat-state-attachments-attachment-status` |
| 3 | Vector store and indexing | `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`, `cpt-cf-mini-chat-algo-attachments-index-document`, `cpt-cf-mini-chat-algo-attachments-background-indexing`, `cpt-cf-mini-chat-state-attachments-chat-vector-store` |
| 4 | Images | `cpt-cf-mini-chat-algo-attachments-generate-thumbnail`, `cpt-cf-mini-chat-algo-attachments-upload-secondary-copy`, `cpt-cf-mini-chat-state-attachments-secondary-copy` |
| 5 | Delete and cleanup | `cpt-cf-mini-chat-flow-attachments-delete-attachment`, `cpt-cf-mini-chat-algo-attachments-soft-delete`, `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`, `cpt-cf-mini-chat-algo-attachments-handle-cleanup`, `cpt-cf-mini-chat-state-attachments-cleanup-status` |
| 6 | Reaper, metrics and registration | `cpt-cf-mini-chat-algo-attachments-reap-abandoned`, `cpt-cf-mini-chat-algo-attachments-record-metrics`, the REST registration DoD |

**Out of scope** (owned by other features): the tool decision that turns ready attachments into `file_search`, `code_interpreter` and `input_image` entries, the `attachment_ids` checks of a send, the citation mapping and the Anthropic image blocks (`cpt-cf-mini-chat-feature-streaming-turns`, reused by reference: `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`, `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`, `cpt-cf-mini-chat-algo-streaming-turns-map-citations`, `cpt-cf-mini-chat-algo-streaming-turns-create-turn`); the `AttachmentSummary` of the message list (`cpt-cf-mini-chat-algo-chat-crud-enrich-messages`); marking attachments `cleanup_status` `pending` on chat delete (`cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`); the chat-cleanup handler that deletes the provider files and the chat vector store of a deleted chat, and the Kubernetes Lease implementation of the leader elector (`cpt-cf-mini-chat-feature-background-workers`); attachment re-linking on retry and edit (`cpt-cf-mini-chat-feature-turn-mutations`). Not implemented per `cpt-cf-mini-chat-adr-document-retrieval-scope`: document summaries (`doc_summary`, `summary_model`, `summary_updated_at` stay NULL), the per-chat chunk cap, immediate exclusion of a deleted document from `file_search`, document search in Anthropic chats, attachment-scoped retrieval (P2) and the provider file reconciliation job (P2). Image quotas and `quota_usage.image_upload_bytes` are not implemented (`cpt-cf-mini-chat-adr-quota-policy-scope`); uploads take no quota reserve. The request gate, PEP, scoped chat and child access, error mapping, provider resolution, OAGW RAG route provisioning, outbox queues, configuration, schema, metric registration and the elector preparation come from `cpt-cf-mini-chat-feature-gear-foundation`.

### 1.2 Purpose

Users need to ask questions about their own contracts, reports, spreadsheets and screenshots. An upload must be validated completely before anything is stored, stored only with the provider through OAGW, indexed so that `file_search` finds it as soon as the upload returns, and recoverable when a request is dropped or indexing stalls. Deleting a file must never break a sent message, and provider files must eventually be removed without blocking the user.

**Requirements**: `cpt-cf-mini-chat-fr-file-upload`, `cpt-cf-mini-chat-fr-image-upload`, `cpt-cf-mini-chat-fr-file-search`, `cpt-cf-mini-chat-fr-code-interpreter`, `cpt-cf-mini-chat-fr-per-chat-doc-limits`, `cpt-cf-mini-chat-fr-attachment-deletion`, `cpt-cf-mini-chat-fr-doc-summary`, `cpt-cf-mini-chat-nfr-rag-scalability`, `cpt-cf-mini-chat-nfr-tenant-isolation`

`cpt-cf-mini-chat-fr-file-search` and `cpt-cf-mini-chat-fr-code-interpreter` are covered for their attachment facet: the vector store, indexing and readiness, and the spreadsheet purpose with its provider file; tool enablement, tool events and quota belong to `cpt-cf-mini-chat-feature-streaming-turns` and `cpt-cf-mini-chat-feature-quota-billing`. `cpt-cf-mini-chat-fr-doc-summary` is traced only to confirm its not-implemented status: `doc_summary` is never written and never returned. `cpt-cf-mini-chat-nfr-rag-scalability` is covered for the per-chat count and size limits and the per-chat vector store; the chunk cap and retrieved-token limit are not implemented. `cpt-cf-mini-chat-nfr-tenant-isolation` is covered for attachment rows, the vector store row and the provider identifiers.

**Principles**: `cpt-cf-mini-chat-principle-owner-only-content`, `cpt-cf-mini-chat-principle-tenant-scoped`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Uploads a document or an image into an own chat, polls the attachment until it is `ready` or `failed`, and deletes an attachment that no sent message references |
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | Runs as the attachment cleanup outbox handler of this feature: deletes the provider file (and the Anthropic secondary copy) of a deleted, abandoned, failed-upload or failed-indexing attachment with retries, and the provider file of an `attachment_orphan_file` message whose id no row carries |
| RAG provider (OpenAI or Azure OpenAI Files and Vector Stores APIs, behind OAGW) | Stores the file, creates the chat vector store, indexes the document and reports its indexing status; in tests an OpenAI-compatible mock behind OAGW answering the `files` and `vector_stores` routes |
| Anthropic Files API (behind OAGW) | Stores and deletes the secondary copy of an image uploaded into a chat whose model is served by an `anthropic_messages` provider |
| Model policy plugin (via the model-policy gateway) | Supplies the catalog entry of the chat's model (`max_file_size_mb`, `tool_support.code_interpreter`, `provider_id`) and the kill switches `disable_images` and `disable_code_interpreter` |
| ToolKit runtime (gear host) | Registers the three routes, runs authentication, license middleware and extractors, hosts the shared outbox that delivers the attachment cleanup messages, and runs the leader-only reaper after `start` |
| Send pipeline (`cpt-cf-mini-chat-feature-streaming-turns`) | Consumes the `ready` rows, provider file ids, secondary file ids and the chat vector store id; never writes attachment rows |
| Chat-deletion cleanup (`cpt-cf-mini-chat-feature-background-workers`) | Owns provider cleanup of every attachment whose chat is soft-deleted; this feature's handler and background task step aside for those rows |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-mini-chat-fr-file-upload`, `cpt-cf-mini-chat-fr-image-upload`, `cpt-cf-mini-chat-fr-file-search`, `cpt-cf-mini-chat-fr-code-interpreter`, `cpt-cf-mini-chat-fr-per-chat-doc-limits`, `cpt-cf-mini-chat-fr-attachment-deletion`, `cpt-cf-mini-chat-fr-doc-summary`, `cpt-cf-mini-chat-nfr-rag-scalability`; UC-002 `cpt-cf-mini-chat-usecase-doc-search`, UC-003 `cpt-cf-mini-chat-usecase-upload-document`, UC-010 `cpt-cf-mini-chat-usecase-upload-image`; §13 open question on the MIME allowlist (resolved)
- **Design**: [DESIGN.md](../DESIGN.md) — §2.2 "OpenAI-Compatible Provider (P1)" (`cpt-cf-mini-chat-constraint-openai-compatible`, "Files API upload field mapping", "File storage"), "Model Capability Constraint (Images)" (`cpt-cf-mini-chat-constraint-model-image-capability`); §3.2 `llm_provider` (`cpt-cf-mini-chat-component-llm-provider`), chat service (`cpt-cf-mini-chat-component-chat-service`), persistence layer (`cpt-cf-mini-chat-component-chat-store`), "upload reaper"; §3.3 "Get Attachment", "Upload Attachment", REST error mapping; §3.5 "LLM Provider" (RAG routes, provider API mapping); §3.6 `cpt-cf-mini-chat-seq-file-upload` with "Thumbnail generation details", "Cleanup on Chat Deletion" ("Cleanup responsibility boundaries", "Attachment cleanup state machine", "Additional invariants"); §3.7 `attachments`, `message_attachments`, `chat_vector_stores` ("Concurrency invariants", "Creation protocol", "Stale placeholder reclaim"); §3.8 per-operation matrix (`upload_attachment`, `read_attachment`, `delete_attachment`); §4 "File Search Tool Availability", "Code Interpreter Tool Availability" (`cpt-cf-mini-chat-design-code-interpreter`), "File Search Retrieval Scope", "Retrieval Invariants", "Vector Store Scope (P1)" (`cpt-cf-mini-chat-design-vector-store-scope`), "Citation File ID and Title Resolution", "Attachment Mutability and Deletion", "Attachment Deletion", "RAG Quality & Scale Controls (P1)" (`cpt-cf-mini-chat-design-rag-quality-controls`), "Metrics (OTLP) and Alerts (P1)"; Appendix B.7, B.8, B.9.2, B.9.3, B.9.5
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.5
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Attachments (all four items; "Attachments are correctly made available to the relevant provider tools" for the attachment data, the tool decision being owned by `cpt-cf-mini-chat-feature-streaming-turns`); Principles & Constraints "Tenant and owner isolation enforced on every resource" (attachment part); Error Mapping & Sanitization "All errors map to the canonical error contract" (attachment routes); Authorization (attachment operations)
- **ADRs**: `cpt-cf-mini-chat-adr-llm-provider-as-library`, `cpt-cf-mini-chat-adr-internal-transport`, `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-multi-provider-adapters`, `cpt-cf-mini-chat-adr-document-retrieval-scope`, `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`
- **Foundation elements reused by reference** (from `cpt-cf-mini-chat-feature-gear-foundation`, not redefined here): request gate `cpt-cf-mini-chat-flow-gear-foundation-request-gate`; PEP `cpt-cf-mini-chat-algo-gear-foundation-authorize` (actions `upload_attachment`, `read_attachment`, `delete_attachment`); owner-scoped chat and child access `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; DomainError to `Problem` mapping `cpt-cf-mini-chat-algo-gear-foundation-map-error`; plugin resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`; provider resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`; OAGW RAG routes `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw` (`inst-create-rag-routes`); outbox queues and the attachment-cleanup placeholder `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`; configuration `cpt-cf-mini-chat-algo-gear-foundation-load-config` (`rag`, `thumbnail`, `upload_reaper`, `cleanup_worker.max_attempts`, `outbox.cleanup_queue_name`); elector preparation and leader-worker spawn `cpt-cf-mini-chat-flow-gear-foundation-gear-start` (`inst-prepare-elector`, `inst-spawn-workers`); background task registry `cpt-cf-mini-chat-flow-gear-foundation-gear-stop`; metric instruments `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`; schema `cpt-cf-mini-chat-dod-gear-foundation-schema`
- **Chat CRUD elements reused by reference** (from `cpt-cf-mini-chat-feature-chat-crud`): the chat soft delete that marks attachments `pending` `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`; the `AttachmentSummary` projection `cpt-cf-mini-chat-algo-chat-crud-enrich-messages`
- **Streaming elements reused by reference** (from `cpt-cf-mini-chat-feature-streaming-turns`, consumers of this feature's data): `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`, `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`, `cpt-cf-mini-chat-algo-streaming-turns-create-turn`, `cpt-cf-mini-chat-algo-streaming-turns-map-citations`, `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`
- **Dependencies**: `cpt-cf-mini-chat-feature-chat-crud`, `cpt-cf-mini-chat-feature-streaming-turns`

**Source precedence**: DESIGN is authoritative for details. Where DESIGN delegates schemas to the generated OpenAPI (`docs/api/api.json`), the schemas `AttachmentDetailDto`, `AttachmentKindDto`, `AttachmentStatusDto`, `ImgThumbnailDto` and the multipart request body (field `file`, binary, required) apply. The gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides).

**REST contract** (paths under `{url_prefix}`, default `/mini-chat`; every route requires authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`; PEP resource type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, `require_constraints` `true`, `resource.id` the chat id; path ids must be UUIDs, otherwise 400 `invalid_path_params`):

| Method and path | PEP action | Request | Success | Domain errors (besides gate, extractor and PEP errors) |
|---|---|---|---|---|
| `POST /v1/chats/{id}/attachments` | `upload_attachment` | `multipart/form-data` with one part named `file` (binary, with a `Content-Type`); other parts ignored; no route body limit | 201, `AttachmentDetail` with `status` `ready`, or `uploaded` when indexing is still running at the deadline; no `Location` header | 404 chat; 400 `INVALID_MODEL`, `BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`, `MISSING_CONTENT_TYPE`, `UNSUPPORTED_CONTENT_TYPE`, `CODE_INTERPRETER_UNAVAILABLE`, `FILE_TOO_LARGE`, `failed_precondition` `FEATURE_DISABLED` (`images`); 409 `provider_mismatch`; 429 `document_limit`, `storage_limit`; 503 `Retry-After: 5` (upload concurrency), 503 `Retry-After: 10` (provider or storage failure, `indexing_failed`); 500 `internal` (policy or provider resolution, database) |
| `GET /v1/chats/{id}/attachments/{attachment_id}` | `read_attachment` | none | 200, `AttachmentDetail` | 404 chat; 404 attachment (unknown, soft-deleted, other chat, other uploader) |
| `DELETE /v1/chats/{id}/attachments/{attachment_id}` | `delete_attachment` | none | 204, no body (also for a repeated delete) | 404 chat; 404 attachment (unknown, other chat, other uploader); 409 `attachment_locked`; 500 `internal` (database, cleanup enqueue) |

`AttachmentDetail` (`AttachmentDetailDto`) always has `id` (UUID), `filename` (string), `content_type` (the stored normalized MIME type), `size_bytes` (integer), `status` (`pending` \| `uploaded` \| `ready` \| `failed`), `kind` (`document` \| `image`) and `created_at` (RFC 3339 UTC). Optional keys are omitted, never sent as `null`: `error_code` (only with `status` `failed`: `upload_failed`, `vector_store_failed`, `indexing_failed`, `upload_abandoned`), `img_thumbnail` (only with `kind` `image`, `status` `ready` and stored thumbnail bytes: `{ "content_type": "image/webp", "width", "height", "data_base64" }`), and `doc_summary` and `summary_updated_at` (never present: document summaries are not implemented). It never carries `provider_file_id`, `secondary_file_id`, a vector store id, `storage_backend`, cleanup fields, purpose flags, `chat_id`, `tenant_id`, `uploaded_by_user_id`, `updated_at` or `deleted_at`.

**Upload limits and routing** (sizes in bytes; KiB = 1024, MiB = 1,048,576):

| Rule | Value |
|------|-------|
| Multipart field | Exactly one part named `file` is read (the first one); parts with other names before it are discarded unread, parts after it are never read |
| Filename | `filename` of the part's `Content-Disposition`; `upload` when absent or empty; longer than 255 characters (Unicode scalar values) is cut to 255 keeping the extension (the text from the last `.` when it is not the first character) |
| Document size limit | `rag.uploaded_file_max_size_kb` × 1024 (default 25 MiB), lowered to the chat model's `max_file_size_mb` × 1,048,576 when that is greater than 0 and smaller |
| Image size limit | `rag.uploaded_image_max_size_kb` × 1024 (default 5 MiB), lowered the same way |
| Per-chat document count | At most `rag.max_documents_per_chat` (default 50) live (`deleted_at IS NULL`), non-`failed` documents, the new one included; images are not counted; 429 `document_limit` |
| Per-chat storage | At most `rag.max_total_upload_mb_per_chat` × 1,048,576 (default 100 MiB) of `size_bytes` over live, non-`failed` attachments of every kind, the new one included; 429 `storage_limit` |
| Concurrent uploads | At most `rag.max_concurrent_uploads` (default 10) in-flight upload requests per process; the next one gets 503 with `Retry-After: 5` without its body being read |
| Purposes | XLSX: `for_code_interpreter` only; other documents: `for_file_search` only; images: neither |
| Code interpreter availability | The chat model's `tool_support.code_interpreter` is `true` and the snapshot's `disable_code_interpreter` is `false`; otherwise `for_code_interpreter` is cleared and a document left with no purpose is rejected with 400 `CODE_INTERPRETER_UNAVAILABLE` |
| Images kill switch | `disable_images` rejects every image upload with 400 `failed_precondition` `violations[{subject: images, type: FEATURE_DISABLED}]` |
| Thumbnail | WebP, fit inside `thumbnail.width` × `thumbnail.height` (default 128 × 128); skipped above `thumbnail.max_bytes` encoded (default 131,072), above `thumbnail.max_decode_bytes` source bytes (default 32 MiB), when the header gives more than `thumbnail.max_pixels` pixels (default 100,000,000) or `width × height × 4` above `thumbnail.max_decode_bytes`; the decoder allocation is capped at `thumbnail.max_decode_bytes` |
| Secondary copy | Images of a chat whose model's provider entry has kind `anthropic_messages`, with at most `thumbnail.max_decode_bytes` bytes |

**MIME allowlist** (PRD §13 resolution; the part's `Content-Type` is lower-cased and its parameters after `;` are dropped; aliases are stored in the first form listed; `application/octet-stream` is replaced by the type of the filename extension, compared case-insensitively, and an unknown extension keeps `application/octet-stream`, which is unsupported):

| Type | Accepted values (stored form first) | Extensions for `application/octet-stream` | Kind | Purpose | Provider extension |
|------|-------------------------------------|-------------------------------------------|------|---------|--------------------|
| PDF | `application/pdf` | `.pdf` | document | `file_search` | `pdf` |
| Word | `application/vnd.openxmlformats-officedocument.wordprocessingml.document` | `.docx` | document | `file_search` | `docx` |
| PowerPoint | `application/vnd.openxmlformats-officedocument.presentationml.presentation` | `.pptx` | document | `file_search` | `pptx` |
| Excel | `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet` | `.xlsx` | document | `code_interpreter` | `xlsx` |
| Plain text | `text/plain` | `.txt` | document | `file_search` | `txt` |
| CSV | `text/csv`, stored as `text/plain` when `rag.allow_csv_upload` is `true` (default), unsupported otherwise | `.csv` | document | `file_search` | `txt` |
| Markdown | `text/markdown`, `text/x-markdown` | `.md`, `.markdown` | document | `file_search` | `md` |
| HTML | `text/html` | `.html`, `.htm` | document | `file_search` | `html` |
| JSON | `application/json` | `.json` | document | `file_search` | `json` |
| Python | `text/x-python`, `text/x-script.python` | `.py` | document | `file_search` | `py` |
| Java | `text/x-java`, `text/x-java-source` | `.java` | document | `file_search` | `java` |
| JavaScript | `text/javascript`, `application/javascript` | `.js`, `.mjs` | document | `file_search` | `js` |
| TypeScript | `application/typescript`, `text/x-typescript` | `.ts` | document | `file_search` | `ts` |
| Rust | `text/x-rust` | `.rs` | document | `file_search` | `rs` |
| Go | `text/x-golang`, `text/x-go` | `.go` | document | `file_search` | `go` |
| C# | `text/x-csharp` | `.cs` | document | `file_search` | `cs` |
| Ruby | `text/x-ruby` | `.rb` | document | `file_search` | `rb` |
| SQL | `application/sql`, `text/x-sql` | `.sql` | document | `file_search` | `sql` |
| PNG | `image/png` | `.png` | image | none | `png` |
| JPEG | `image/jpeg`, `image/jpg` | `.jpg`, `.jpeg` | image | none | `jpg` |
| WebP | `image/webp` | `.webp` | image | none | `webp` |
| GIF | `image/gif` | `.gif` | image | none | `gif` |

**Storage wire contract** (every call goes through `ServiceGatewayClientV1::proxy_request` to the RAG routes that `inst-create-rag-routes` provisions on the storage provider's upstream: `POST {prefix}/files` with path suffix disabled, the other routes with suffix append; the storage provider is the chat model's provider entry, or its `rag_provider`; `alias` is that entry's alias for the request tenant; `{prefix}` is `/v1` for `storage_kind` `openai` and `/openai` for `azure`, where every request also carries the query `api-version={api_version}` of that entry; no credential header is ever set, OAGW injects it):

| Operation | Request | Body | Success |
|-----------|---------|------|---------|
| File upload | `POST /{alias}{prefix}/files` | `multipart/form-data`: part `purpose` = `assistants`, then part `file` with filename `{chat_id}_{attachment_id}.{ext}` (provider extension of the stored type) and `Content-Type` the stored type | 2xx JSON with a non-empty string `id` (the `provider_file_id`) |
| Vector store create | `POST /{alias}{prefix}/vector_stores` | JSON `{ "name": "mini-chat-{chat_id}" }` | 2xx JSON with a non-empty string `id` (the `vector_store_id`) |
| Add file to store | `POST /{alias}{prefix}/vector_stores/{vector_store_id}/files` | JSON `{ "file_id": provider_file_id, "attributes": { "attachment_id": "<attachment uuid>" } }` | 2xx JSON with `status` |
| Indexing status | `GET /{alias}{prefix}/vector_stores/{vector_store_id}/files/{provider_file_id}` | none | 2xx JSON with `status` (`in_progress`, `completed`, `failed`, `cancelled`; absent means `in_progress`) |
| File delete | `DELETE /{alias}{prefix}/files/{provider_file_id}` | none | 2xx or 404 |
| Orphan store delete (lost CAS only) | `DELETE /{alias}{prefix}/vector_stores/{vector_store_id}` | none | 2xx or 404 |
| Secondary upload (Anthropic) | `POST /{anthropic_alias}/v1/files` with headers `anthropic-version: 2023-06-01` and `anthropic-beta: files-api-2025-04-14` | `multipart/form-data` with only the part `file` (same filename and type), no `purpose` | 2xx JSON with a non-empty string `id` (the `secondary_file_id`) |
| Secondary delete (Anthropic) | `DELETE /{alias}/v1/files/{secondary_file_id}` with the same two headers | none | 2xx or 404 |

JSON requests carry `Content-Type: application/json` and `Accept: application/json`; provider ids are placed as single percent-encoded path segments; provider response bodies are read up to 1 MiB. The secondary upload uses the `POST /v1/files` RAG route that the foundation provisions on the Anthropic entry's upstream (its `storage_kind` is `openai`).

**Attachment cleanup payload** (queue `outbox.cleanup_queue_name`, default `mini-chat.attachment_cleanup`, partitioned by `tenant_id`; a JSON object with exactly these keys):

| Key | Value |
|-----|-------|
| `event_type` | `attachment_deleted` (user delete), `attachment_upload_abandoned` (reaper), `attachment_indexing_failed` (indexing failure in the background task or in the upload request), `attachment_upload_failed` (an upload request failed a row that has a provider file with `upload_failed` or `vector_store_failed`), `attachment_orphan_file` (row-less variant: a provider file id that no row can carry, see `cpt-cf-mini-chat-algo-attachments-handle-cleanup`) |
| `tenant_id`, `chat_id`, `attachment_id` | UUID strings of the row (for `attachment_orphan_file`, of the upload that created the file; the row may be gone, deleted, reaped or owned by chat cleanup) |
| `provider_file_id` | The row's provider file id, or `null` when the upload never reached the provider; never `null` for `attachment_upload_failed` and `attachment_orphan_file` |
| `vector_store_id` | Always `null` (the handler never reads it) |
| `storage_backend` | The row's `storage_backend` label |
| `attachment_kind` | `document` or `image` |
| `deleted_at` | Enqueue time, RFC 3339 UTC |
| `secondary_ref` | `{ "file_id", "provider_kind": "anthropic", "upstream_alias" }` for an `attachment_deleted` event of a row whose secondary copy is `uploaded` and whose Anthropic alias resolved at delete time, and for an `attachment_upload_failed` event of a row whose secondary copy is `uploaded` (alias from the upload context's Anthropic secondary target); otherwise `null` |

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| Check order of an upload | PEP, chat load (404), chat-model resolution (`INVALID_MODEL`, 500), the concurrency permit (503), then multipart framing, classification (type, `disable_images`, code interpreter), size, the `provider_mismatch` pre-check and the insert with the per-chat limits | DESIGN requires the chat and model checks before the body is read; the sequence diagram puts MIME, kill switches and the size count before the insert; the permit is taken before the body so that a rejected request costs no body read |
| Multipart reason fields (closes foundation finding K-TCS-001) | `field_violations[].field` is `content_type` for `BOUNDARY_REQUIRED` and `MISSING_CONTENT_TYPE`, `multipart` for `MULTIPART_ERROR`, `file` for `MISSING_FILE`; `UNSUPPORTED_CONTENT_TYPE` and `CODE_INTERPRETER_UNAVAILABLE` use `file`; `FILE_TOO_LARGE` uses `content_length`; all carry the attachment resource type | DESIGN REST error table names the field per reason; the foundation mapping used `file` for every multipart reason |
| Per-chat limit violation shape | `context.violations[0]` is `{ subject: document_limit, description: document_limit }` or `{ subject: storage_limit, description: storage_limit }` | PRD names the violation subject `document_limit` / `storage_limit`; the foundation mapping used `documents` / `storage` as subject; repeating the reason as description keeps it machine-readable in both fields |
| Content buffering and limit timing | The file content is read before the insert with a byte counter and kept in memory, never more than the effective limit plus one chunk; `FILE_TOO_LARGE`, `document_limit` and `storage_limit` are therefore decided before any row or provider call exists, and the row `error_code` values `file_too_large` and `storage_limit_exceeded` listed in DESIGN §3.7 are never produced | The sequence diagram counts bytes and enforces the limits at the insert, before the provider upload; the size of a streamed body is unknown until it is read. Memory is bounded by `rag.max_concurrent_uploads` × the limit (250 MiB with defaults) |
| Images above `thumbnail.max_decode_bytes` | Such an image (possible only when the image limit is configured above it) gets no thumbnail and no secondary copy, as DESIGN states; its bytes are still held for the provider upload | DESIGN's observable rule is kept; holding the bytes follows from the buffering interpretation above |
| No vision check at upload | An image upload does not check `VISION_INPUT` of the chat model; vision is checked per turn on the effective model (`inst-image-guards` of `cpt-cf-mini-chat-flow-streaming-turns-send-message`) | DESIGN's upload error table has no vision row, and `cpt-cf-mini-chat-constraint-model-image-capability` binds the check to the effective model after the quota cascade, which an upload does not have |
| `disable_file_search` at upload | Has no effect on uploads; documents are still indexed | DESIGN lists only `disable_images` and `disable_code_interpreter` as upload switches; `disable_file_search` gates the tool per turn |
| `max_file_size_mb` of 0 | A catalog `max_file_size_mb` of 0 imposes no per-model cap; only the gear limit applies | The field is required by the decoder; a literal minimum of 0 would reject every upload into the chat |
| `provider_mismatch` scope | Checked only for `for_file_search` documents: before the insert against an existing `chat_vector_stores` row, and again inside the vector store resolution (a race then fails the row with `vector_store_failed`) | Only these uploads use the chat vector store; images and spreadsheets never touch it |
| Request deadline | Every provider call of the upload request (file upload, secondary upload, store create, add file, status reads) is bounded by the time left until the upload start plus 25 s; a write call cut by it fails its step, a status read cut by it ends the wait with `uploaded` | DESIGN fixes 25 s for the indexing wait so that the answer leaves before the api-gateway 30 s timeout; the same bound on the writes keeps that guarantee |
| Transient status read errors | Provider 5xx and gateway or transport failures (including OAGW 502, 503 and 504 problems and timeouts before the deadline) are transient; every other error, including 429 and an unparsable body, fails the indexing | DESIGN lists "provider 5xx, gateway failure" as transient and "any other status read error" as failed |
| Vector store create body | `{ "name": "mini-chat-{chat_id}" }`, no metadata | DESIGN fixes only the file attribute `attachment_id`; a name helps operators find the store |
| Upload state guards | `pending` to `uploaded` and `uploaded` to `ready` are single-statement CAS updates that also require `deleted_at IS NULL` and `cleanup_status IS NULL`; when the upload request loses the `pending` to `uploaded` one (the attachment or the chat was deleted, or the reaper took the row meanwhile), it enqueues the row-less `attachment_orphan_file` cleanup message for the new provider file and answers 404 (attachment type, or chat type when the chat was deleted); a lost `uploaded` to `ready` CAS needs nothing, the id is already on the row | A delete of a `pending` row enqueues a payload without `provider_file_id`, and chat cleanup only knows ids already stored; the durable message closes that orphan window with retries |
| Durable cleanup of failed uploads (closes findings A5-SRPT-001, A5-SRPT-002) | Whenever an upload request fails a row that has a provider file (stored on the row, or held in memory after a failed `uploaded` write), whatever the error code, one write-first transaction writes `failed`, the `provider_file_id`, `cleanup_status` `pending`, and enqueues the cleanup message with the same payload as delete: `event_type` `attachment_indexing_failed` for `indexing_failed` (existing DESIGN reason) and the new `attachment_upload_failed` for `upload_failed` and `vector_store_failed` (no DESIGN reason fits). This replaces DESIGN §3.6's in-request "best effort, fire-and-forget, not retried" delete after a failed indexing | A fire-and-forget delete leaves no record when it fails or the process stops, and a database error on the `ready` write left the stored file with no deleter at all. The background indexing failure already uses this mechanism in DESIGN; the end state (one provider `DELETE`, the row `failed`) is unchanged, only made durable |
| Row-less orphan file message | `event_type` `attachment_orphan_file` carries a provider file id that no row can carry (lost `pending` to `uploaded` CAS; an upload failure whose row was taken meanwhile). The handler deletes the file without reading or writing the chat or attachment row, also when the row is gone, not `pending` or in a soft-deleted chat, and bounds its retries by the outbox delivery attempt count against `cleanup_worker.max_attempts` | DESIGN has no durable record for such a file (Additional invariants 4 defers orphans to the P2 reconciliation job); chat cleanup only knows ids stored on rows, so the variant must not step aside for a deleted chat |
| Lock rule on delete | Any `message_attachments` row for the attachment locks it, also when its message was later soft-deleted by a turn mutation | DESIGN: "referenced by any submitted message"; the association is immutable history |
| Delete of a row that already has a cleanup status | The row is soft-deleted and no new cleanup message is enqueued (a `pending` row already has its message; `done` and `failed` rows need none) | Avoids duplicate provider deletes; `cleanup_status` keeps its state machine |
| `secondary_ref` resolution | At delete time, best effort: the chat model's provider entry is resolved from the current snapshot (one call bounded by `POLICY_PLUGIN_CALL_TIMEOUT`); when it fails or the kind is not `anthropic_messages`, `secondary_ref` is `null`, a warning names the attachment and `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}` is recorded | The schema stores no alias for the secondary copy; DESIGN deletes the secondary file "when the upstream alias could be resolved" |
| Cleanup handler edge cases | For row-bearing events, a row that cannot be found is `Reject` (the `attachment_orphan_file` variant never looks the row up); a `storage_backend` label that no configured provider entry carries counts as a failed attempt (`last_cleanup_error` `storage backend not configured`); a redelivery for a row already `done` or `failed` is `Ok` without a provider call | DESIGN covers malformed payloads and the attempt budget only; these keep the handler total and idempotent |
| Leader election of the reaper | The reaper asks the foundation elector for leadership under the Lease `mini-chat-upload-reaper`; the Kubernetes Lease implementation is delivered by `cpt-cf-mini-chat-feature-background-workers` (foundation deferral), and until then the no-op elector lets every replica scan, which the CAS makes safe | DESIGN B.9.5 and `cpt-cf-mini-chat-adr-runtime-consistency-limitations` (double processing prevented by the CAS in any case) |
| Security context of background calls | Calls made outside a request (background indexing, cleanup handler, best-effort deletes spawned after the response) use the S2S security context obtained at `start`; calls inside the request use the request's context | The request context can expire during a 10-minute wait; the foundation reconcile task uses the same S2S context |
| Thumbnail details | No enlargement of an image already inside the box; each side at least 1 pixel; the first frame of an animated GIF; `data_base64` is standard Base64 with padding | DESIGN says "fit inside" without upscaling rules |
| Empty files | A 0-byte file passes validation and is sent to the provider like any other | DESIGN defines no minimum size; the provider decides, and a provider refusal is `upload_failed` |
| Per-chat limit races (residual risk) | On PostgreSQL (READ COMMITTED) two concurrent uploads into the same chat can both pass the limit checks and exceed a limit by at most the concurrent uploads; on SQLite the write-first insert serializes them. Owner: mini-chat gear maintainers | Closing the gap needs a chat-row lock that would also bump or block chat activity; `cpt-cf-mini-chat-adr-runtime-consistency-limitations` accepts store-level gaps of this kind |
| Delete versus send race (residual risk) | On PostgreSQL a send that links an attachment while it is being deleted can commit a `message_attachments` row for a soft-deleted attachment; the message list then omits it and its citations are dropped. Owner: mini-chat gear maintainers | The send checks `deleted_at IS NULL` without a row lock (`cpt-cf-mini-chat-algo-streaming-turns-create-turn`); both outcomes stay consistent for the user |
| Orphan vector store delete stays best effort (residual risk) | When the winner loses the `vector_store_id` CAS (its placeholder was reclaimed), the new provider store is deleted best effort (bounded by `PROVIDER_DELETE_TIMEOUT`, not retried, outcome logged) and no cleanup message is enqueued; a failed delete leaves one empty provider vector store. Owner: mini-chat gear maintainers | DESIGN §3.7 "Creation protocol" 4c mandates the best-effort delete, and DESIGN "Attachment Deletion" states the attachment cleanup handler makes no Vector Stores API call and the payload's `vector_store_id` is always `null`. The store holds no file (the CAS precedes the add-file call), so the debt is one empty store per lost race of a placeholder older than 120 s |
| Database unavailable during a failed upload (residual risk) | When the failure transaction of `cpt-cf-mini-chat-algo-attachments-fail-upload` or the `attachment_orphan_file` enqueue itself fails, a provider file id held only in memory gets one last-resort best-effort delete; when that also fails, the file is a provider orphan. A file id already stored on the row is not at risk: the row stays `uploaded` and the reaper fails it with the cleanup message. Owner: mini-chat gear maintainers | The outbox lives in the same database, so no durable record can be written while it is unavailable; DESIGN Additional invariants 4 assigns such orphans to the P2 provider file reconciliation job |

**External-call timeouts** (every call to a dependency that can stall is bounded):

| Constant | Value | Call in this feature | Outcome on timeout |
|----------|-------|----------------------|--------------------|
| `PDP_EVALUATION_TIMEOUT` (foundation) | 5 s | PEP evaluation of `upload_attachment`, `read_attachment`, `delete_attachment` | 503 `service_unavailable`, `Retry-After: 5` |
| `REGISTRY_QUERY_TIMEOUT` (foundation) | 10 s | Model-policy plugin resolution | Upload: 500 `internal` before the body is read; delete: `secondary_ref` `null` |
| `POLICY_PLUGIN_CALL_TIMEOUT` (foundation) | 10 s | `get_current_policy_version`, `get_policy_snapshot` of the upload context and of the delete's `secondary_ref` | Upload: 500 `internal`; delete: `secondary_ref` `null` |
| `UPLOAD_INDEXING_DEADLINE` (DESIGN, fixed) | upload start + 25 s | Every provider call of the upload request and the indexing wait | Write step fails (503 `Retry-After: 10`); status wait ends with 201 `uploaded` |
| `BACKGROUND_INDEXING_LIMIT` (DESIGN, fixed) | 10 min in rounds of 20 s | Status reads of the background task, each bounded by the time left in its round | `failed` / `indexing_failed`, metric `result="timeout"` |
| `PROVIDER_DELETE_TIMEOUT` (feature constant) | 10 s | Each provider or secondary file delete (handler and best-effort deletes) and the orphan store delete | Handler: failed attempt (`Retry` or terminal); best effort: logged only |
| Set-ready retry (DESIGN, fixed) | 4 attempts, 1 s, 2 s, 4 s apart | The background `ready` write | Row stays `uploaded`, metric `result="set_ready_failed"`, reaper finishes it |
| Vector store loser polls (DESIGN, fixed count) | 5 polls, waits 250 ms doubling, never past the deadline | Re-read of a placeholder row | 503 `Retry-After: 10`, row `vector_store_failed` |

Two `PROVIDER_DELETE_TIMEOUT` calls (primary and secondary) fit inside the 30 s default lease of the attachment cleanup queue. The background heartbeat interval (20 s) is checked at compile time to be at most half of the minimum `upload_reaper.stale_after_secs` (60 s).

**Transaction discipline** (normative): no provider call, plugin call or thumbnail decode runs inside a database transaction, and no connection is held across a provider call. Every multi-statement transaction of this feature starts with a write (on SQLite the transaction may instead be begun `IMMEDIATE`), so it never upgrades from a read lock to a write lock: the pending insert starts with the `attachments` INSERT and checks the limits afterwards; the soft delete starts with the conditional `attachments` UPDATE and checks the lock afterwards; the background indexing failure, the upload failure of a row that has a provider file and the reaper start with their CAS UPDATE and then enqueue the cleanup message; the `attachment_orphan_file` enqueue of a lost `pending` to `uploaded` CAS runs alone in its own transaction. Every other write is a single autocommit statement: the `pending` to `uploaded` and `uploaded` to `ready` CAS updates, the failure update of a row without a provider file, the heartbeat, the secondary-copy columns, the vector store placeholder INSERT, its CAS and its reclaim DELETE, and every cleanup-handler update. Closures are deterministic: ids, timestamps and payload bytes are computed before the transaction and captured by value, because a lock or serialization conflict (`SQLITE_BUSY`, PostgreSQL `40001` / `40P01`) re-runs the whole transaction, at most 3 attempts in total with the 20 ms / 100 ms backoff. Unique violations are surfaced as the typed `DomainError::UniqueViolation` and resolved by the caller (the vector store placeholder), never retried.

**Audit and metrics**: attachments emit no audit event in P1 (`cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`). The feature records the upload, indexing, reaper and file-cleanup instruments listed in `cpt-cf-mini-chat-algo-attachments-record-metrics` with their bounded labels only.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-mini-chat-usecase-upload-document`, `cpt-cf-mini-chat-usecase-upload-image`, `cpt-cf-mini-chat-usecase-doc-search`

### Upload Attachment

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-upload-attachment`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A supported document is stored with the provider, added to the chat vector store and indexed within the deadline: 201 with `status` `ready`.
- A document still `in_progress` at the deadline: 201 with `status` `uploaded`; the background task makes it `ready` or `failed` later.
- An XLSX file with the code interpreter available is stored with the provider only: 201 `ready`, `for_code_interpreter` `true`.
- An image is stored with the provider (plus the Anthropic copy in Anthropic chats) and gets a WebP thumbnail when possible: 201 `ready`, with `img_thumbnail` when one was generated.

**Error Scenarios**:
- Gate, extractor and PEP errors of `cpt-cf-mini-chat-flow-gear-foundation-request-gate`: 401, 403 `LICENSE_FEATURE_REQUIRED`, 403 `AUTHZ_DENIED`, 503 `Retry-After: 5`, 400 `invalid_path_params`.
- Before any row exists: 404 chat, 400 `INVALID_MODEL`, 503 `Retry-After: 5` (concurrency), 400 multipart reasons, 400 `UNSUPPORTED_CONTENT_TYPE`, 400 `FEATURE_DISABLED` `images`, 400 `CODE_INTERPRETER_UNAVAILABLE`, 400 `FILE_TOO_LARGE`, 409 `provider_mismatch`, 429 `document_limit` / `storage_limit`, 500.
- After the row exists: 503 `Retry-After: 10` with the row `failed` (`upload_failed`, `vector_store_failed`, `indexing_failed`), 409 `provider_mismatch` with the row `failed` (`vector_store_failed`), 404 when the attachment or chat was deleted during the upload, 500 on a database failure.

**Steps**:
1. [x] - `p1` - User calls `POST {url_prefix}/v1/chats/{id}/attachments` with a `multipart/form-data` body through the request gate (see `cpt-cf-mini-chat-flow-gear-foundation-request-gate`); the route sets no body limit and the handler reads the raw body as a stream; the api-gateway `defaults.body_limit_bytes` caps the body (413 from the gateway) - `inst-call-upload`
2. [x] - `p1` - Take the current time as the upload start; the request deadline is the upload start plus 25 s (`UPLOAD_INDEXING_DEADLINE`) - `inst-upload-start`
3. [x] - `p1` - Authorize action `upload_attachment` on the chat resource with `resource.id` the path chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-upload`
4. [x] - `p1` - Load the chat through the compiled scope (see `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`); **IF** it is missing, soft-deleted or foreign, **RETURN** 404 `not_found` with `gts.cf.core.mini_chat.chat.v1~`; no body byte has been read - `inst-load-chat`
5. [x] - `p1` - Resolve the upload context (see `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`); **IF** it returns `INVALID_MODEL`, **RETURN** 400 `invalid_argument` with `field_violations[model].reason` `INVALID_MODEL`; **IF** it fails otherwise, **RETURN** 500 `internal`; no body byte has been read - `inst-resolve-context`
6. [x] - `p1` - Try to take one permit of the per-process upload semaphore of `rag.max_concurrent_uploads` permits without waiting; **IF** none is free, **RETURN** 503 `service_unavailable` with `Retry-After: 5` and `context.retry_after_seconds` 5 without reading the body. The permit is released on every exit of this flow, including a dropped request; the background indexing task never holds one - `inst-acquire-permit`
7. [x] - `p1` - Read the multipart request up to the headers of the `file` part (see `cpt-cf-mini-chat-algo-attachments-read-multipart`); **IF** it returns a multipart issue, **RETURN** 400 `invalid_argument` with that reason and field - `inst-read-multipart`
8. [x] - `p1` - Classify the file (see `cpt-cf-mini-chat-algo-attachments-classify-file`); **IF** it returns an error, **RETURN** its `Problem`: 400 `UNSUPPORTED_CONTENT_TYPE`, 400 `failed_precondition` `FEATURE_DISABLED` `images`, or 400 `CODE_INTERPRETER_UNAVAILABLE` - `inst-classify`
9. [x] - `p1` - Read the file content with the effective size limit of its kind (see `cpt-cf-mini-chat-algo-attachments-read-file-content`); **IF** it returns `FILE_TOO_LARGE`, **RETURN** 400 `out_of_range` with `field_violations[content_length].reason` `FILE_TOO_LARGE`; **IF** it returns `MULTIPART_ERROR`, **RETURN** 400 with that reason; no row exists and no provider call was made - `inst-read-content`
10. [x] - `p1` - **IF** `for_file_search` is `true`, DB: SELECT `chat_vector_stores` WHERE `tenant_id` is the chat tenant and `chat_id` is the chat; **IF** a row exists whose `provider` differs from the storage backend label of the upload context, **RETURN** 409 `already_exists` with `resource_name` `provider_mismatch`; nothing is stored - `inst-precheck-provider`
11. [x] - `p1` - Insert the `pending` row under the per-chat limits (see `cpt-cf-mini-chat-algo-attachments-insert-pending`); **IF** it returns `document_limit` or `storage_limit`, **RETURN** 429 `resource_exhausted` with that violation; **IF** it fails otherwise, **RETURN** 500 `internal`; nothing is stored - `inst-insert-pending`
12. [x] - `p1` - Upload the file to the provider (see `cpt-cf-mini-chat-algo-attachments-upload-provider-file`); **IF** it returns `upload_failed`, fail the upload with `upload_failed` and **RETURN** its `Problem` (see `cpt-cf-mini-chat-algo-attachments-fail-upload`); **IF** it returns `taken`, **RETURN** 404 with the attachment resource type, or the chat resource type when the chat was soft-deleted; **IF** it returns a database error, fail the upload with `upload_failed` (cause database), passing the provider file id it returns so that the failure records it durably, and **RETURN** 500 `internal` - `inst-upload-file`
13. [x] - `p1` - **IF** the kind is `image`, the upload context names an Anthropic secondary target and the size is at most `thumbnail.max_decode_bytes`, upload the secondary copy (see `cpt-cf-mini-chat-algo-attachments-upload-secondary-copy`); its outcome never fails the upload - `inst-secondary-copy`
14. [x] - `p1` - **MATCH** the processing path of the row - `inst-match-path`
    - [x] - `p1` - **CASE** a `for_file_search` document: resolve the chat vector store (see `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`); **IF** it fails, fail the upload with `vector_store_failed` and **RETURN** its `Problem` (409 `provider_mismatch` for a mismatch, otherwise 503 `Retry-After: 10`); then index the document (see `cpt-cf-mini-chat-algo-attachments-index-document`) - `inst-case-file-search`
    - [x] - `p1` - **CASE** a `for_code_interpreter`-only document: no vector store call; the row goes to `ready` (the provider file id is the code interpreter container input) - `inst-case-code-interpreter`
    - [x] - `p1` - **CASE** an image: generate the thumbnail (see `cpt-cf-mini-chat-algo-attachments-generate-thumbnail`); a skipped or failed thumbnail never fails the upload and sets no `error_code` - `inst-case-image`
15. [x] - `p1` - **IF** the indexing outcome is `failed`, fail the upload with `indexing_failed` (the row gets `cleanup_status` `pending` and the `attachment_indexing_failed` cleanup message in one transaction; the cleanup handler deletes the provider file with retries) and **RETURN** 503 `service_unavailable` with `Retry-After: 10` and the generic detail; `indexing_failed` is not in the response body - `inst-if-indexing-failed`
16. [x] - `p1` - **IF** the indexing outcome is `pending_at_deadline`, spawn the background indexing continuation on the gear's background task registry (see `cpt-cf-mini-chat-algo-attachments-background-indexing`), keep the row `uploaded`, record the upload metrics and **RETURN** 201 with the `AttachmentDetail` of the row (`status` `uploaded`) - `inst-if-indexing-pending`
17. [x] - `p1` - Move the row to `ready` with the thumbnail of step 14 for an image (see `cpt-cf-mini-chat-algo-attachments-mark-ready`); **IF** it returns `taken`, **RETURN** 404 with the attachment resource type, or the chat resource type when the chat was soft-deleted; the provider file id is already on the row, so the attachment cleanup or the chat cleanup that took the row deletes the file; **IF** it returns a database error, fail the upload with `upload_failed` (cause database), which marks the row `failed` with `cleanup_status` `pending` and enqueues the `attachment_upload_failed` cleanup message for its stored provider file (and its secondary copy) in one transaction, and **RETURN** 500 `internal` - `inst-mark-ready`
18. [x] - `p1` - Record the upload metrics (see `cpt-cf-mini-chat-algo-attachments-record-metrics`); every exit after classification records them with its result - `inst-record-upload`
19. [x] - `p1` - **RETURN** 201 with the `AttachmentDetail` of the ready row (see `cpt-cf-mini-chat-algo-attachments-project-detail`); no `Location` header is set - `inst-return-created`

### Get Attachment

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-get-attachment`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with the current `AttachmentDetail`; a client polls it after a 201 `uploaded` until `status` is `ready` or `failed`; a row that failed during upload or was reaped shows `status` `failed` and its `error_code`.

**Error Scenarios**:
- Gate and PEP errors; a non-UUID path id is 400 `invalid_path_params`.
- 404 with the chat type for a missing, deleted or foreign chat; 404 with the attachment type for an unknown or soft-deleted attachment, one of another chat, or one uploaded by another user.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/chats/{id}/attachments/{attachment_id}` through the request gate; the path extractor requires both ids to be UUIDs - `inst-call-get`
2. [x] - `p1` - Authorize action `read_attachment` with `resource.id` the chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-read`
3. [x] - `p1` - Load the chat and the child attachment (see `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`): DB: SELECT `attachments` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the loaded chat, `id` is the path value and `deleted_at IS NULL` - `inst-load-attachment`
4. [x] - `p1` - **IF** no row is found, or its `uploaded_by_user_id` is not the requesting user - `inst-if-attachment-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `context.resource_type` `gts.cf.core.mini_chat.attachment.v1~`; another uploader's row is indistinguishable from an unknown id - `inst-return-attachment-404`
5. [x] - `p1` - **RETURN** 200 with the `AttachmentDetail` of the row (see `cpt-cf-mini-chat-algo-attachments-project-detail`) - `inst-return-detail`

### Delete Attachment

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-attachments-delete-attachment`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 204 for an unreferenced attachment: the row is soft-deleted, excluded at once from the chat metadata, the `file_search` inclusion check and citations, and one cleanup message is enqueued in the same transaction.
- 204 again for an attachment that is already soft-deleted, without a new cleanup message.

**Error Scenarios**:
- Gate and PEP errors; a non-UUID path id is 400 `invalid_path_params`.
- 404 with the chat type for a missing, deleted or foreign chat; 404 with the attachment type for an unknown attachment, one of another chat or one uploaded by another user (also on a repeated delete).
- 409 `already_exists` with `resource_name` `attachment_locked` when a sent message references the attachment; the row is unchanged.
- 500 `internal` on a database failure or a failed enqueue (an oversized payload included); the row is unchanged.

**Steps**:
1. [x] - `p1` - User calls `DELETE {url_prefix}/v1/chats/{id}/attachments/{attachment_id}` through the request gate; the path extractor requires both ids to be UUIDs - `inst-call-delete`
2. [x] - `p1` - Authorize action `delete_attachment` with `resource.id` the chat id (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`); **IF** it returns an error, **RETURN** 403 `AUTHZ_DENIED` or 503 with `Retry-After: 5` - `inst-authorize-delete`
3. [x] - `p1` - Load the chat through the compiled scope (see `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`); **IF** it is missing, soft-deleted or foreign, **RETURN** 404 with `gts.cf.core.mini_chat.chat.v1~` - `inst-load-chat-for-delete`
4. [x] - `p1` - DB: SELECT `attachments` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the loaded chat and `id` is the path value, soft-deleted rows included - `inst-select-for-delete`
5. [x] - `p1` - **IF** no row is found, or its `uploaded_by_user_id` is not the requesting user (checked before the idempotency check) - `inst-if-delete-missing`
   1. [x] - `p1` - **RETURN** 404 `not_found` with `gts.cf.core.mini_chat.attachment.v1~` - `inst-return-delete-404`
6. [x] - `p1` - **IF** the row's `deleted_at` is set - `inst-if-already-deleted`
   1. [x] - `p1` - **RETURN** 204 without any write and without enqueueing a cleanup message - `inst-return-repeat-204`
7. [x] - `p1` - **IF** the row's `secondary_status` is `uploaded` with a `secondary_file_id`, resolve the `secondary_ref` best effort as stated in the §1.4 "`secondary_ref` resolution" interpretation; this runs before the transaction - `inst-resolve-secondary-ref`
8. [x] - `p1` - Soft-delete the attachment (see `cpt-cf-mini-chat-algo-attachments-soft-delete`); **IF** it returns `locked`, **RETURN** 409 `already_exists` with `resource_name` `attachment_locked`; **IF** it fails, **RETURN** 500 `internal` - `inst-soft-delete`
9. [x] - `p1` - **RETURN** 204 with no body; the response never waits for provider cleanup - `inst-return-204`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Upload Context Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`

**Input**: The loaded chat (`chats.model`), the request tenant, the `rag` and `thumbnail` configuration, the validated provider entries

**Output**: The upload context: the storage target (storage provider id, `storage_kind`, storage backend label, alias for the tenant, `api_version`), the Anthropic secondary target or none, `disable_images`, the code interpreter availability, the document and image size limits in bytes, `allow_csv_upload`, `thumbnail.max_decode_bytes`; or `INVALID_MODEL`; or an internal error

**Steps**:
1. [x] - `p1` - Read the current policy version and the snapshot of that version through the model-policy gateway (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`), no cache, each call bounded by `POLICY_PLUGIN_CALL_TIMEOUT`; **IF** a call fails or times out, **RETURN** internal error - `inst-read-snapshot`
2. [x] - `p1` - Find the `model_catalog` entry whose `id` equals `chats.model`, enabled or not (an upload runs no quota cascade); **IF** there is none, **RETURN** `INVALID_MODEL` - `inst-find-chat-model`
3. [x] - `p1` - Resolve the entry's `provider_id` for the request tenant (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-provider`), giving the provider kind, its alias and the storage provider id (`rag_provider` or the provider itself); **IF** it fails, **RETURN** internal error - `inst-resolve-chat-provider`
4. [x] - `p1` - Look up the storage provider entry: its `storage_kind`, its `storage_backend` label (the provider id when not configured), its `api_version` and its alias for the tenant (the tenant override alias when one exists); **IF** the entry is missing, **RETURN** internal error - `inst-resolve-storage-target`
5. [x] - `p1` - **IF** the chat provider's kind is `anthropic_messages`, set the secondary target to that provider's alias for the tenant; **ELSE** set none - `inst-secondary-target`
6. [x] - `p1` - Set `disable_images` and `disable_code_interpreter` from the snapshot kill switches; set the code interpreter availability to the entry's `tool_support.code_interpreter` and not `disable_code_interpreter` - `inst-capabilities`
7. [x] - `p1` - Set the document limit to `rag.uploaded_file_max_size_kb` times 1024 and the image limit to `rag.uploaded_image_max_size_kb` times 1024; **IF** the entry's `max_file_size_mb` is greater than 0, lower each limit to `max_file_size_mb` times 1,048,576 when that is smaller - `inst-size-limits`
8. [x] - `p1` - **RETURN** the upload context; no fallback provider or limit is ever used - `inst-return-context`

### Multipart Reading

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-read-multipart`

**Input**: The request `Content-Type` header, the raw body stream

**Output**: The declared content type and filename of the `file` part and the stream of its content; or a multipart issue with its field

**Steps**:
1. [x] - `p1` - **IF** the `Content-Type` is not `multipart/form-data` with a non-empty `boundary` parameter - `inst-if-no-boundary`
   1. [x] - `p1` - **RETURN** `BOUNDARY_REQUIRED` with field `content_type` - `inst-return-boundary`
2. [x] - `p1` - **WHILE** no part named exactly `file` (case-sensitive) has been found - `inst-while-parts`
   1. [x] - `p1` - Read the headers of the next part; **IF** the body ends, **RETURN** `MISSING_FILE` with field `file` - `inst-if-no-file`
   2. [x] - `p1` - **IF** the framing is malformed, the body is truncated or a part's headers cannot be parsed, **RETURN** `MULTIPART_ERROR` with field `multipart` - `inst-if-malformed`
   3. [x] - `p1` - **IF** the part's name is not `file`, read its content and discard it without keeping it - `inst-discard-part`
3. [x] - `p1` - **IF** the `file` part has no `Content-Type` header or an empty one, **RETURN** `MISSING_CONTENT_TYPE` with field `content_type` - `inst-if-no-part-type`
4. [x] - `p1` - Set the filename to the part's `Content-Disposition` `filename` (or `filename*`), or to `upload` when it is absent or empty; **IF** it is longer than 255 characters, cut it to 255 characters keeping the extension as stated in the §1.4 limits table - `inst-filename`
5. [x] - `p1` - **RETURN** the declared content type, the filename and the content stream of the `file` part; the parts after it are never read - `inst-return-part`

### File Classification and Purpose Routing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-classify-file`

**Input**: The declared content type, the filename, the upload context

**Output**: The stored content type, `attachment_kind`, `for_file_search`, `for_code_interpreter`, the provider extension; or `UNSUPPORTED_CONTENT_TYPE`, `FEATURE_DISABLED` `images`, `CODE_INTERPRETER_UNAVAILABLE`

**Steps**:
1. [x] - `p1` - Normalize the declared type: lower-case it, drop everything from the first `;`, and trim whitespace - `inst-normalize-type`
2. [x] - `p1` - **IF** the type is `application/octet-stream`, replace it with the type of the filename extension from the §1.4 MIME allowlist (case-insensitive); an unknown or missing extension keeps `application/octet-stream` - `inst-infer-from-extension`
3. [x] - `p1` - **IF** the type is `text/csv`: **IF** `rag.allow_csv_upload` is `true`, set it to `text/plain`; **ELSE** **RETURN** `UNSUPPORTED_CONTENT_TYPE` - `inst-csv`
4. [x] - `p1` - Replace an alias with the stored form of its allowlist row; **IF** the type is not in the allowlist, **RETURN** `UNSUPPORTED_CONTENT_TYPE` (400 `invalid_argument`, field `file`) - `inst-check-allowlist`
5. [x] - `p1` - Set `attachment_kind` to `image` for `image/png`, `image/jpeg`, `image/webp` and `image/gif`, otherwise to `document` - `inst-kind`
6. [x] - `p1` - **IF** the kind is `image` and `disable_images` is `true`, **RETURN** `FEATURE_DISABLED` with subject `images` (400 `failed_precondition`) - `inst-images-kill-switch`
7. [x] - `p1` - Set the purposes from the allowlist row: XLSX gives `for_code_interpreter` `true` and `for_file_search` `false`; every other document gives `for_file_search` `true` and `for_code_interpreter` `false`; an image gives both `false` - `inst-purposes`
8. [x] - `p1` - **IF** `for_code_interpreter` is `true` and the code interpreter is not available, set `for_code_interpreter` to `false` - `inst-filter-code-interpreter`
9. [x] - `p1` - **IF** the kind is `document` and both purposes are now `false`, **RETURN** `CODE_INTERPRETER_UNAVAILABLE` (400 `invalid_argument`, field `file`, attachment resource type) - `inst-return-ci-unavailable`
10. [x] - `p1` - **RETURN** the stored type, the kind, both purpose flags and the provider extension of the row - `inst-return-classification`

### Bounded File Content Read

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-read-file-content`

**Input**: The content stream of the `file` part, the effective limit of the kind

**Output**: The file bytes and `size_bytes`; or `FILE_TOO_LARGE`; or `MULTIPART_ERROR`

**Steps**:
1. [x] - `p1` - **WHILE** the part has more content, read the next chunk and add its length to a byte counter as it arrives - `inst-count-bytes`
   1. [x] - `p1` - **IF** the counter exceeds the limit, stop reading, drop every byte read so far and **RETURN** `FILE_TOO_LARGE`; the rest of the body is never read - `inst-if-over-limit`
   2. [x] - `p1` - **IF** the read fails or the part ends without its closing boundary, **RETURN** `MULTIPART_ERROR` - `inst-if-read-failed`
   3. [x] - `p1` - Append the chunk to the in-memory content, which therefore never exceeds the limit - `inst-keep-chunk`
2. [x] - `p1` - **RETURN** the bytes and the counter as `size_bytes`; an empty file is accepted - `inst-return-content`

### Pending Row Insert and Per-Chat Limits

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-insert-pending`

**Input**: The chat, the requesting user, the filename, the classification, `size_bytes`, the storage backend label, `rag.max_documents_per_chat`, `rag.max_total_upload_mb_per_chat`

**Output**: The inserted `pending` row; or `document_limit`; or `storage_limit`; or a database error

**Transaction discipline** (normative): see §1.4; the transaction starts with the INSERT.

**Steps**:
1. [x] - `p1` - Before the transaction, generate the attachment id (UUID v4) and take the current time as `now`; the closure captures them by value - `inst-pregenerate-attachment`
2. [x] - `p1` - Open the transaction write-first: DB: INSERT `attachments` with `id`, `tenant_id` the chat tenant, `chat_id`, `uploaded_by_user_id` the requesting user, `filename`, `content_type` the stored type, `size_bytes`, `storage_backend` the label, `provider_file_id` NULL, `status` `pending`, `error_code` NULL, `attachment_kind`, `for_file_search`, `for_code_interpreter`, `doc_summary`, thumbnail columns, `summary_model` and `summary_updated_at` NULL, `cleanup_status` NULL, `cleanup_attempts` 0, `created_at` and `updated_at` `now`, `deleted_at` NULL, `secondary_status` `not_attempted`, `secondary_file_id` and `secondary_provider_kind` NULL - `inst-insert-row`
3. [x] - `p1` - **IF** the kind is `document`, DB: SELECT the count of the chat's `attachments` with `attachment_kind` `document`, `deleted_at IS NULL` and `status` other than `failed`, the new row included; **IF** it is greater than `rag.max_documents_per_chat`, roll back and **RETURN** `document_limit` - `inst-check-document-limit`
4. [x] - `p1` - DB: SELECT the sum of `size_bytes` of the chat's `attachments` with `deleted_at IS NULL` and `status` other than `failed`, every kind and the new row included; **IF** it is greater than `rag.max_total_upload_mb_per_chat` times 1,048,576, roll back and **RETURN** `storage_limit` - `inst-check-storage-limit`
5. [x] - `p1` - Commit; the `pending` row is visible through `GET` from now on - `inst-commit-insert`
6. [x] - `p1` - **IF** the transaction failed otherwise (database error, conflict retries exhausted), **RETURN** a database error; nothing is left behind - `inst-return-insert-error`
7. [x] - `p1` - **RETURN** the row - `inst-return-pending`

### Storage Request Construction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-build-storage-request`

**Input**: The storage target or the Anthropic secondary target, the operation, the ids it needs, the file bytes and type for an upload

**Output**: The HTTP request for `proxy_request`

**Steps**:
1. [x] - `p1` - Set the prefix to `/{alias}/v1` for `storage_kind` `openai` and to `/{alias}/openai` for `azure`; for `azure` add the query `api-version` with the entry's `api_version` to every request - `inst-storage-prefix`
2. [x] - `p1` - **MATCH** the operation - `inst-match-operation`
   - [x] - `p1` - **CASE** file upload: `POST {prefix}/files`, a `multipart/form-data` body with the part `purpose` = `assistants` followed by the part `file` named `{chat_id}_{attachment_id}.{ext}` with `Content-Type` the stored type, streamed from the held bytes - `inst-case-file-upload`
   - [x] - `p1` - **CASE** vector store create: `POST {prefix}/vector_stores` with JSON `{ name: mini-chat-{chat_id} }` - `inst-case-store-create`
   - [x] - `p1` - **CASE** add file: `POST {prefix}/vector_stores/{vector_store_id}/files` with JSON `file_id` the provider file id and `attributes` holding only `attachment_id` - `inst-case-add-file`
   - [x] - `p1` - **CASE** indexing status: `GET {prefix}/vector_stores/{vector_store_id}/files/{provider_file_id}` - `inst-case-status-read`
   - [x] - `p1` - **CASE** file delete: `DELETE {prefix}/files/{provider_file_id}`; orphan store delete: `DELETE {prefix}/vector_stores/{vector_store_id}` - `inst-case-deletes`
   - [x] - `p1` - **CASE** secondary upload: `POST /{anthropic_alias}/v1/files` with only the part `file` and no `purpose`; secondary delete: `DELETE /{alias}/v1/files/{secondary_file_id}`; both with the headers `anthropic-version` `2023-06-01` and `anthropic-beta` `files-api-2025-04-14` - `inst-case-secondary`
3. [x] - `p1` - Set `Content-Type: application/json` and `Accept: application/json` on JSON requests; place every provider id as one percent-encoded path segment; never set a credential header (OAGW injects it, `cpt-cf-mini-chat-constraint-no-credentials`) - `inst-storage-headers`
4. [x] - `p1` - **RETURN** the request - `inst-return-storage-request`

### Provider File Upload

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-upload-provider-file`

**Input**: The `pending` row, the file bytes, the storage target, the request `SecurityContext`, the request deadline

**Output**: The row in `uploaded` with its `provider_file_id`; or `upload_failed`; or `taken`; or a database error with the returned provider file id

**Steps**:
1. [x] - `p1` - API: `proxy_request` with the request `SecurityContext` and the file upload request (see `cpt-cf-mini-chat-algo-attachments-build-storage-request`), bounded by the time left until the deadline; the write is never retried and carries no idempotency key - `inst-call-files`
2. [x] - `p1` - **IF** the call fails, times out, answers non-2xx, or its JSON body has no non-empty string `id` - `inst-if-files-failed`
   1. [x] - `p1` - Log the cause with the attachment and chat ids (the raw provider text only in the log) and **RETURN** `upload_failed` - `inst-return-upload-failed`
3. [x] - `p1` - DB: UPDATE `attachments` SET `status` `uploaded`, `provider_file_id` the returned `id`, `updated_at` now WHERE `id` and `tenant_id` match, `status` is `pending`, `deleted_at IS NULL` and `cleanup_status IS NULL` (single-statement CAS) - `inst-set-uploaded`
4. [x] - `p1` - **IF** no row was updated (the attachment or its chat was deleted, or the reaper took the row, during the upload; no row can carry the new id) - `inst-if-uploaded-lost`
   1. [x] - `p1` - Enqueue the row-less `attachment_orphan_file` cleanup message with the new provider file id, the row's tenant, chat and attachment ids, the storage backend label, the kind and `secondary_ref` `null` (see `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`) to `outbox.cleanup_queue_name` partitioned by `tenant_id`, in its own transaction; the cleanup handler deletes the file with retries whatever the row's state - `inst-enqueue-orphan-file`
   2. [x] - `p1` - **IF** the enqueue fails, log it with the attachment id and spawn a last-resort best-effort delete of the new provider file with the S2S context (bounded by `PROVIDER_DELETE_TIMEOUT`, not retried, outcome logged; §1.4 "Database unavailable during a failed upload" residual risk) - `inst-orphan-enqueue-failed`
   3. [x] - `p1` - **RETURN** `taken`, noting whether the chat is soft-deleted - `inst-return-taken`
5. [x] - `p1` - **IF** the update failed with a database error, **RETURN** a database error carrying the new provider file id; no delete is spawned here: the flow fails the row with `upload_failed` through `cpt-cf-mini-chat-algo-attachments-fail-upload`, which records the id durably, and answers 500 - `inst-if-uploaded-db-error`
6. [x] - `p1` - **RETURN** the `uploaded` row - `inst-return-uploaded`

### Secondary Anthropic Copy

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-upload-secondary-copy`

**Input**: The `uploaded` image row, the image bytes, the Anthropic secondary target, the request `SecurityContext`, the request deadline

**Output**: The secondary-copy columns updated; never an error for the upload

**Steps**:
1. [x] - `p1` - DB: UPDATE `attachments` SET `secondary_status` `pending`, `secondary_provider_kind` `anthropic`, `updated_at` now WHERE `id` and `tenant_id` match - `inst-set-secondary-pending`
2. [x] - `p1` - API: `proxy_request` with the request `SecurityContext` and the secondary upload request (see `cpt-cf-mini-chat-algo-attachments-build-storage-request`), bounded by the time left until the deadline - `inst-call-secondary`
3. [x] - `p1` - **IF** it answers 2xx with a non-empty string `id`, DB: UPDATE `attachments` SET `secondary_file_id` the `id`, `secondary_status` `uploaded`, `updated_at` now WHERE `id` and `tenant_id` match - `inst-set-secondary-uploaded`
4. [x] - `p1` - **ELSE** DB: UPDATE `attachments` SET `secondary_status` `failed` WHERE `id` and `tenant_id` match, and log a warning with the attachment id; the upload continues and the Anthropic adapter drops this image with a warning on a later turn - `inst-set-secondary-failed`
5. [x] - `p1` - **RETURN**; a database error on these columns is logged and never fails the upload - `inst-return-secondary`

### Chat Vector Store Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`

**Input**: The chat tenant and id, the storage target (backend label), the request `SecurityContext`, the request deadline

**Output**: The chat's `vector_store_id`; or `provider_mismatch`; or `vector_store_failed`

**Steps**:
1. [x] - `p1` - DB: SELECT `chat_vector_stores` WHERE `tenant_id` is the chat tenant and `chat_id` is the chat (never by `id` or `vector_store_id` alone) - `inst-select-store`
2. [x] - `p1` - **IF** a row with a non-NULL `vector_store_id` exists: **IF** its `provider` differs from the backend label, **RETURN** `provider_mismatch`; **ELSE** **RETURN** its `vector_store_id` - `inst-fast-path`
3. [x] - `p1` - **IF** a row with a NULL `vector_store_id` exists and its `created_at` is more than 120 s old, DB: DELETE `chat_vector_stores` WHERE `id` is that row and `vector_store_id IS NULL` (stale placeholder reclaim) and **GO TO** step 1, at most once per upload - `inst-reclaim-stale`
4. [x] - `p1` - **IF** a row with a NULL `vector_store_id` exists (creation in progress), **GO TO** step 9 - `inst-if-placeholder`
5. [x] - `p1` - DB: INSERT `chat_vector_stores` with a new `id` (UUID v4), `tenant_id`, `chat_id`, `vector_store_id` NULL, `provider` the backend label, `file_count` 0, `created_at` now, auto-committed; **IF** it fails with `DomainError::UniqueViolation` on `(tenant_id, chat_id)`, **GO TO** step 9 (this request lost and never calls the provider) - `inst-insert-placeholder`
6. [x] - `p1` - API: `proxy_request` with the vector store create request (see `cpt-cf-mini-chat-algo-attachments-build-storage-request`), bounded by the time left until the deadline; **IF** it fails, times out, answers non-2xx or returns no non-empty string `id`, DB: DELETE the placeholder WHERE `id` is the row and `vector_store_id IS NULL` (best effort) and **RETURN** `vector_store_failed` - `inst-create-store`
7. [x] - `p1` - DB: UPDATE `chat_vector_stores` SET `vector_store_id` the returned `id` WHERE `id` is the placeholder row and `vector_store_id IS NULL` (compare-and-set); **IF** no row was updated (the placeholder was reclaimed meanwhile), spawn a best-effort delete of the new, still empty provider store (bounded by `PROVIDER_DELETE_TIMEOUT`, not retried, outcome logged; kept best effort as DESIGN mandates, see the §1.4 "Orphan vector store delete stays best effort" residual risk) and **GO TO** step 9 - `inst-cas-store-id`
8. [x] - `p1` - **RETURN** the new `vector_store_id`; exactly one provider store is bound to the chat - `inst-return-new-store`
9. [x] - `p1` - **FOR EACH** of at most 5 polls, waiting 250 ms, 500 ms, 1 s, 2 s and 4 s and never past the deadline - `inst-for-each-poll`
   1. [x] - `p1` - DB: SELECT the chat's `chat_vector_stores` row by `tenant_id` and `chat_id`; **IF** it has a non-NULL `vector_store_id`, apply the provider check of step 2 and **RETURN** its result - `inst-poll-store`
10. [x] - `p1` - **RETURN** `vector_store_failed` (503) - `inst-return-poll-exhausted`

### Document Indexing Within the Request

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-index-document`

**Input**: The `uploaded` row, the `vector_store_id`, the storage target, the request `SecurityContext`, the request deadline

**Output**: `ready`, `failed`, or `pending_at_deadline`

**Steps**:
1. [x] - `p1` - API: `proxy_request` with the add-file request (see `cpt-cf-mini-chat-algo-attachments-build-storage-request`), bounded by the time left until the deadline; the write is never retried - `inst-add-file`
2. [x] - `p1` - **IF** the call fails, times out, answers non-2xx or its body is not JSON, **RETURN** `failed` - `inst-if-add-failed`
3. [x] - `p1` - Set the status to the response `status`, or `in_progress` when the field is absent; set the next wait to 250 ms - `inst-initial-status`
4. [x] - `p1` - **WHILE** the status is `in_progress` - `inst-while-in-progress`
   1. [x] - `p1` - **IF** the deadline has passed, **RETURN** `pending_at_deadline` - `inst-if-deadline`
   2. [x] - `p1` - Wait for the next wait or the time left until the deadline, whichever is shorter; double the next wait, capped at 2 s - `inst-wait`
   3. [x] - `p1` - API: `proxy_request` with the indexing status request, bounded by the time left until the deadline; a read cut by the deadline continues with step 4.1 - `inst-read-status`
   4. [x] - `p1` - **IF** the read failed transiently (provider 5xx, gateway or transport failure), keep the status `in_progress`, keep the error as the last transient error (the first one is logged at `warn`) and continue - `inst-if-transient`
   5. [x] - `p1` - **ELSE IF** the read failed otherwise (any other non-2xx including 429, a body that is not JSON), **RETURN** `failed` - `inst-if-read-error`
   6. [x] - `p1` - **ELSE** set the status to the response `status`, or `in_progress` when the field is absent - `inst-update-status`
5. [x] - `p1` - **IF** the status is `completed`, **RETURN** `ready` - `inst-return-indexed`
6. [x] - `p1` - **RETURN** `failed` for `failed`, `cancelled` or any other value - `inst-return-index-failed`

### Background Indexing Continuation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-background-indexing`

**Input**: The attachment tenant, chat and id, the `vector_store_id`, the `provider_file_id`, the storage target, the S2S security context, the gear stop token

**Output**: The row moved to `ready` or `failed`, or left unchanged; one `mini_chat_attachment_background_indexing` count

**Steps**:
1. [x] - `p1` - Start the 10-minute window (`BACKGROUND_INDEXING_LIMIT`); the task is not persisted, runs on the gear's background task registry and holds no upload permit - `inst-start-window`
2. [x] - `p1` - **WHILE** the window is open and the gear stop token is not cancelled - `inst-while-window`
   1. [x] - `p1` - DB: UPDATE `attachments` SET `updated_at` now WHERE `id` and `tenant_id` match, `status` is `uploaded`, `deleted_at IS NULL` and `cleanup_status IS NULL` (heartbeat, once per 20 s round); **IF** no row was updated (deleted, no longer `uploaded`, or its chat was deleted), **RETURN** without changes and without a metric; a database error is logged and the round continues - `inst-heartbeat`
   2. [x] - `p1` - Poll the indexing status with the S2S context during the rest of the round, waiting 250 ms doubling up to 5 s between reads, each read bounded by the time left in the round; a transient error or a response without `status` keeps polling (the last transient error is kept, the first logged at `warn`) - `inst-poll-round`
   3. [x] - `p1` - **IF** a read returns `completed`, **GO TO** step 4 - `inst-if-bg-completed`
   4. [x] - `p1` - **IF** a read returns `failed`, `cancelled` or another value, or fails non-transiently, set the result to `failed` and **GO TO** step 5 - `inst-if-bg-failed`
3. [x] - `p1` - **IF** the stop token was cancelled, **RETURN** without changes (the row stays `uploaded` and the reaper finishes it); **ELSE** set the result to `timeout`, log the failure with the last transient error and **GO TO** step 5 - `inst-if-window-ended`
4. [x] - `p1` - Move the row to `ready` (see `cpt-cf-mini-chat-algo-attachments-mark-ready`), making at most 4 attempts 1 s, 2 s and 4 s apart while the write fails with a database error - `inst-bg-mark-ready`
   1. [x] - `p1` - **IF** it returns `taken`, **RETURN** without changes (the deleting path owns the provider file) - `inst-if-bg-taken`
   2. [x] - `p1` - **IF** every attempt failed, record `result="set_ready_failed"` and **RETURN**; the row stays `uploaded` and the reaper later fails it with `upload_abandoned`, which deletes the provider file - `inst-if-set-ready-failed`
   3. [x] - `p1` - Record `result="ready"` and **RETURN** - `inst-bg-ready`
5. [x] - `p1` - Open a transaction write-first: DB: UPDATE `attachments` SET `status` `failed`, `error_code` `indexing_failed`, `cleanup_status` `pending`, `cleanup_updated_at` now, `updated_at` now WHERE `id` and `tenant_id` match, `status` is `uploaded`, `deleted_at IS NULL` and `cleanup_status IS NULL`; **IF** no row was updated, roll back and **RETURN** without a metric - `inst-bg-fail-row`
6. [x] - `p1` - In the same transaction, enqueue the cleanup message with `event_type` `attachment_indexing_failed` (see `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`) and commit; there is no inline provider delete - `inst-bg-enqueue`
7. [x] - `p1` - **IF** the transaction fails, log it with the attachment id; the row stays `uploaded` and the reaper finishes it - `inst-if-bg-tx-failed`
8. [x] - `p1` - Record `result="failed"` or `result="timeout"` and **RETURN** - `inst-bg-return-failed`

### Thumbnail Generation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-generate-thumbnail`

**Input**: The image bytes, `size_bytes`, the `thumbnail` configuration

**Output**: The WebP bytes with width and height, or none

**Steps**:
1. [x] - `p1` - Run every step below on a blocking worker thread, never on the threads that serve requests - `inst-blocking-thread`
2. [x] - `p1` - **IF** `size_bytes` is greater than `thumbnail.max_decode_bytes`, **RETURN** none - `inst-skip-large-source`
3. [x] - `p1` - Read the source width and height from the image header; **IF** they cannot be read, or `width × height` is greater than `thumbnail.max_pixels`, or `width × height × 4` is greater than `thumbnail.max_decode_bytes`, **RETURN** none - `inst-header-screening`
4. [x] - `p1` - Decode the image (the first frame of a GIF) with the decoder allocation limited to `thumbnail.max_decode_bytes`; **IF** decoding fails or needs more memory, **RETURN** none - `inst-decode`
5. [x] - `p1` - Scale by the smaller of `thumbnail.width` divided by the width, `thumbnail.height` divided by the height, and 1 (never enlarged), keeping the aspect ratio without cropping; each side is rounded and at least 1 pixel - `inst-resize`
6. [x] - `p1` - Encode the result as WebP; **IF** encoding fails or the encoded bytes exceed `thumbnail.max_bytes`, **RETURN** none (no retry with lower quality) - `inst-encode-webp`
7. [x] - `p1` - **RETURN** the bytes, width and height; the thumbnail is stored only in the database and never sent to a provider - `inst-return-thumbnail`

### Ready Transition

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-mark-ready`

**Input**: The attachment tenant and id, the thumbnail or none (images only)

**Output**: `ready`, `taken` (with whether the chat is soft-deleted), or a database error

**Steps**:
1. [x] - `p1` - DB: UPDATE `attachments` SET `status` `ready`, `updated_at` now and, for an image with a thumbnail, `img_thumbnail`, `img_thumbnail_width`, `img_thumbnail_height` WHERE `id` and `tenant_id` match, `status` is `uploaded`, `provider_file_id IS NOT NULL`, `deleted_at IS NULL` and `cleanup_status IS NULL` (single-statement CAS) - `inst-set-ready`
2. [x] - `p1` - **IF** one row was updated, **RETURN** `ready` - `inst-return-ready`
3. [x] - `p1` - **IF** the update failed with a database error, **RETURN** a database error - `inst-return-ready-db-error`
4. [x] - `p1` - DB: SELECT the chat's `deleted_at` by `tenant_id` and `chat_id`, and **RETURN** `taken` with that flag - `inst-return-ready-taken`
5. [x] - `p1` - This algorithm is the only writer of `status` `ready`: a `ready` row always has its `provider_file_id`; a `for_file_search` row is `ready` only after its file reported `completed` in the chat vector store, whose `chat_vector_stores` row is bound; an image is `ready` only after the secondary copy (when due) and the thumbnail attempt; `doc_summary` is never written - `inst-ready-invariant`

### Upload Failure Handling

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-fail-upload`

**Input**: The row, the error code (`upload_failed`, `vector_store_failed`, `indexing_failed`), the cause (provider failure, `provider_mismatch`, database error), the provider file id held in memory when a failed `uploaded` write could not store it on the row, the upload context (storage backend label, Anthropic secondary target), the S2S security context

**Output**: The `Problem` of the upload

**Transaction discipline** (normative): see §1.4; when there is a provider file, the transaction starts with the CAS UPDATE.

**Steps**:
1. [x] - `p1` - Before any write, take `now` and set the cleanup file id to the row's `provider_file_id`, or else to the provider file id held in memory; **IF** there is one, encode (see `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`) the cleanup payload with `event_type` `attachment_indexing_failed` for `indexing_failed` and `attachment_upload_failed` for `upload_failed` and `vector_store_failed`, `provider_file_id` the cleanup file id, `deleted_at` `now`, and `secondary_ref` from the row's `secondary_file_id` and the upload context's Anthropic alias when `secondary_status` is `uploaded`; **IF** the id is held only in memory, also encode the row-less `attachment_orphan_file` payload of the same id; the closure captures them by value. Whatever the error code, a provider file (or secondary copy) is never left without a durable deleter - `inst-prepare-failure`
2. [x] - `p1` - **IF** there is no cleanup file id (the provider upload itself failed), DB: UPDATE `attachments` SET `status` `failed`, `error_code` the code, `updated_at` now WHERE `id` and `tenant_id` match, `status` is `pending` or `uploaded` and `deleted_at IS NULL` (single statement); `cleanup_status` stays NULL; a failed update is logged, and the reaper finishes the row - `inst-set-failed`
3. [x] - `p1` - **ELSE** open a transaction write-first: DB: UPDATE `attachments` SET `status` `failed`, `error_code` the code, `provider_file_id` the cleanup file id, `cleanup_status` `pending`, `cleanup_updated_at` `now`, `updated_at` `now` WHERE `id` and `tenant_id` match, `status` is `pending` or `uploaded`, `deleted_at IS NULL` and `cleanup_status IS NULL` (CAS) - `inst-fail-with-cleanup`
   1. [x] - `p1` - **IF** one row was updated, enqueue the cleanup payload to `outbox.cleanup_queue_name` partitioned by `tenant_id` in the same transaction and commit; there is no inline provider delete, the cleanup handler deletes the file (and the secondary copy) with retries - `inst-enqueue-failure-cleanup`
   2. [x] - `p1` - **ELSE IF** the cleanup file id was held only in memory (the row was deleted, reaped or claimed by chat cleanup meanwhile, so no row can carry it), enqueue the `attachment_orphan_file` payload in the same transaction and commit - `inst-enqueue-failure-orphan`
   3. [x] - `p1` - **ELSE** roll back; the id is stored on the row and the path that took it (user delete, chat cleanup or reaper) owns its deletion - `inst-failure-row-taken`
4. [x] - `p1` - **IF** the transaction of step 3 fails (database error, conflict retries exhausted), log it with the attachment id: a cleanup file id stored on the row keeps the row `uploaded`, and the reaper later fails it with `upload_abandoned` and enqueues its cleanup message; a cleanup file id held only in memory gets a last-resort best-effort delete with the S2S context (bounded by `PROVIDER_DELETE_TIMEOUT`, not retried, outcome logged; §1.4 "Database unavailable during a failed upload" residual risk) - `inst-best-effort-delete`
5. [x] - `p1` - Log the cause with the attachment and chat ids; provider text, provider ids and driver messages stay in the log - `inst-log-failure`
6. [x] - `p1` - **MATCH** the cause - `inst-match-cause`
   - [x] - `p1` - **CASE** `provider_mismatch`: **RETURN** 409 `already_exists` with `resource_name` `provider_mismatch` - `inst-case-mismatch`
   - [x] - `p1` - **CASE** database error: **RETURN** 500 `internal` - `inst-case-db`
   - [x] - `p1` - **DEFAULT**: **RETURN** 503 `service_unavailable` with `Retry-After: 10`, `context.retry_after_seconds` 10 and the generic detail; the `error_code` is only on the row - `inst-case-storage`

### Attachment Detail Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-project-detail`

**Input**: An `attachments` row

**Output**: The `AttachmentDetail` JSON object

**Steps**:
1. [x] - `p1` - Set `id`, `filename`, `content_type`, `size_bytes`, `status`, `kind` (from `attachment_kind`) and `created_at` (RFC 3339 UTC) - `inst-required-fields`
2. [x] - `p1` - **IF** `status` is `failed` and `error_code` is set, add `error_code` - `inst-error-code`
3. [x] - `p1` - **IF** `kind` is `image`, `status` is `ready` and thumbnail bytes are stored, add `img_thumbnail` with `content_type` `image/webp`, `width`, `height` and `data_base64` the standard Base64 of the bytes - `inst-thumbnail-field`
4. [x] - `p1` - Omit `doc_summary` and `summary_updated_at` (always null, `cpt-cf-mini-chat-adr-document-retrieval-scope`) and every other absent optional key - `inst-omit-optional`
5. [x] - `p1` - Never project `provider_file_id`, `secondary_file_id`, `secondary_status`, `storage_backend`, purpose flags, cleanup columns, `chat_id`, `tenant_id`, `uploaded_by_user_id`, `updated_at` or `deleted_at`, and never a vector store id - `inst-non-exposure`
6. [x] - `p1` - **RETURN** the object - `inst-return-projection`

### Attachment Soft Delete

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-soft-delete`

**Input**: The live row, the `secondary_ref` or none, the current time

**Output**: `deleted`, `locked`, or an error

**Transaction discipline** (normative): see §1.4; the transaction starts with the conditional UPDATE.

**Steps**:
1. [x] - `p1` - Before the transaction, take `now` and encode the `attachment_deleted` payload of the row with `deleted_at` `now` and the `secondary_ref` (see `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`); the closure captures them by value - `inst-prepare-delete`
2. [x] - `p1` - Open the transaction write-first: DB: UPDATE `attachments` SET `deleted_at` `now`, `updated_at` `now`, `cleanup_status` `pending`, `cleanup_updated_at` `now` WHERE `id`, `tenant_id` and `chat_id` match, `deleted_at IS NULL` and `cleanup_status IS NULL` - `inst-delete-with-cleanup`
3. [x] - `p1` - **IF** no row was updated, DB: UPDATE `attachments` SET `deleted_at` `now`, `updated_at` `now` WHERE `id`, `tenant_id` and `chat_id` match, `deleted_at IS NULL` and `cleanup_status IS NOT NULL` (a row whose cleanup is already owned); **IF** that updates no row either, roll back and **RETURN** `deleted` (a concurrent delete won) - `inst-delete-owned-cleanup`
4. [x] - `p1` - DB: SELECT whether any `message_attachments` row has this `attachment_id` and `chat_id` (any message, soft-deleted messages included) - `inst-check-lock`
5. [x] - `p1` - **IF** one exists, roll back and **RETURN** `locked`; the row is unchanged - `inst-return-locked`
6. [x] - `p1` - **IF** step 2 updated the row, enqueue the payload to `outbox.cleanup_queue_name` partitioned by `tenant_id` in the same transaction - `inst-enqueue-delete`
7. [x] - `p1` - Commit; from now on the row is excluded from `GET`, the message list, the `file_search` inclusion check and the citation map, while the provider file stays searchable until the cleanup handler deletes it (`cpt-cf-mini-chat-adr-document-retrieval-scope`) - `inst-commit-delete`
8. [x] - `p1` - **IF** the enqueue or the transaction fails (an oversized payload included), roll back and **RETURN** an internal error (500); the row is unchanged - `inst-return-delete-error`
9. [x] - `p1` - **RETURN** `deleted` - `inst-return-deleted`

### Attachment Cleanup Payload

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`

**Input**: The event type, the row, the enqueue time, the `secondary_ref` or none

**Output**: The payload bytes and the partition, or a decode result for the handler

**Steps**:
1. [x] - `p1` - Build a JSON object with exactly the keys of the §1.4 "Attachment cleanup payload" table: `event_type`, `tenant_id`, `chat_id`, `attachment_id`, `provider_file_id` (string or `null`), `vector_store_id` `null`, `storage_backend`, `attachment_kind`, `deleted_at` (RFC 3339 UTC) and `secondary_ref` - `inst-build-payload`
2. [x] - `p1` - Set `secondary_ref` only for `attachment_deleted` with a resolved reference and for `attachment_upload_failed` of a row whose secondary copy is `uploaded`; `attachment_upload_abandoned`, `attachment_indexing_failed` and `attachment_orphan_file` always carry `null`; `attachment_upload_failed` and `attachment_orphan_file` always carry a non-null `provider_file_id` - `inst-secondary-ref-rule`
3. [x] - `p1` - Select the partition of `outbox.cleanup_queue_name` by `tenant_id` (`outbox.num_partitions`) - `inst-partition-by-tenant`
4. [x] - `p1` - For the handler, decode the same shape: an unknown `event_type`, a missing required key, a non-UUID id or a wrong type is a malformed payload - `inst-decode-payload`
5. [x] - `p1` - **RETURN** the bytes and the partition, or the decoded payload - `inst-return-payload`

### Attachment Cleanup Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-handle-cleanup`

**Input**: One message of `outbox.cleanup_queue_name` (payload and delivery attempt count), the S2S security context, `cleanup_worker.max_attempts`, the validated provider entries

**Output**: `Ok`, `Retry` or `Reject` for the shared outbox

**Steps**:
1. [x] - `p1` - Decode the payload (see `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`); **IF** it is malformed, log it and **RETURN** `Reject` (dead-lettered) - `inst-decode-message`
   1. [x] - `p1` - **IF** the `event_type` is `attachment_orphan_file`, **GO TO** step 11 (row-less variant); every other event type (`attachment_deleted`, `attachment_upload_abandoned`, `attachment_indexing_failed`, `attachment_upload_failed`) continues with step 2 - `inst-if-orphan-variant`
2. [x] - `p1` - DB: SELECT `chats` WHERE `tenant_id` and `id` match the payload, soft-deleted rows included; **IF** the read fails, **RETURN** `Retry`; **IF** the chat is soft-deleted or missing, **RETURN** `Ok` without action (chat cleanup owns its files) - `inst-check-chat`
3. [x] - `p1` - DB: SELECT `attachments` WHERE `tenant_id`, `chat_id` and `id` match the payload, soft-deleted rows included; **IF** the read fails, **RETURN** `Retry`; **IF** no row exists, log it and **RETURN** `Reject` - `inst-load-row`
4. [x] - `p1` - **IF** the row's `cleanup_status` is `done` or `failed`, **RETURN** `Ok` (a redelivery) - `inst-if-terminal`
5. [x] - `p1` - **IF** the payload's `provider_file_id` is `null`, **GO TO** step 9 - `inst-if-no-file`
6. [x] - `p1` - Find the provider entry whose storage backend label equals the payload's `storage_backend` (configuration order), with its alias for the payload tenant; API: `proxy_request` with the S2S context and the file delete request, bounded by `PROVIDER_DELETE_TIMEOUT` - `inst-delete-primary`
7. [x] - `p1` - **IF** no entry carries the label, or the call fails, times out or answers neither 2xx nor 404 (any 5xx and any other 4xx alike) - `inst-if-delete-failed`
   1. [x] - `p1` - DB: UPDATE `attachments` SET `cleanup_attempts` plus 1, `last_cleanup_error` a short sanitized cause (`http <status>`, `timeout`, `gateway error`, `storage backend not configured`), `cleanup_updated_at` now WHERE the row is still `cleanup_status` `pending`; **IF** the update fails, **RETURN** `Retry` without counting the attempt - `inst-record-attempt`
   2. [x] - `p1` - **IF** `cleanup_attempts` reached `cleanup_worker.max_attempts`, DB: UPDATE `cleanup_status` `failed` and `cleanup_updated_at` now, record `mini_chat_cleanup_failed{resource_type="file"}` and **RETURN** `Reject` (terminal provider-file debt, dead-lettered) - `inst-if-attempts-exhausted`
   3. [x] - `p1` - **ELSE** record `mini_chat_cleanup_retry{resource_type="file",reason="provider_error"}` and **RETURN** `Retry` (the shared outbox applies backoff) - `inst-return-retry`
8. [x] - `p1` - **IF** the payload has a `secondary_ref`, API: `proxy_request` with the secondary delete request on its `upstream_alias`, bounded by `PROVIDER_DELETE_TIMEOUT`; 2xx and 404 are success; a failure is logged with the attachment id, records `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}` and does not block step 9 - `inst-delete-secondary`
9. [x] - `p1` - DB: UPDATE `attachments` SET `cleanup_status` `done`, `cleanup_updated_at` now WHERE the row is `cleanup_status` `pending`; **IF** the update fails, **RETURN** `Retry` (the repeated delete is harmless, 404 counts as success) - `inst-mark-done`
10. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type="file"}` when a provider file was deleted and **RETURN** `Ok`; no Vector Stores API call is ever made by this handler - `inst-return-ok`
11. [x] - `p1` - Row-less variant (`attachment_orphan_file`): no `chats` or `attachments` row is read and no cleanup column is written; a missing row, a row in any `status` or `cleanup_status` (NULL, `pending`, `done`, `failed`), a soft-deleted row and a soft-deleted chat all lead to the same provider deletes, because no row carries this id and chat cleanup cannot know it - `inst-orphan-variant`
    1. [x] - `p1` - Find the provider entry of the payload's `storage_backend` and API: `proxy_request` with the S2S context and the file delete request of the payload's `provider_file_id`, bounded by `PROVIDER_DELETE_TIMEOUT`, as in step 6 - `inst-orphan-delete-primary`
    2. [x] - `p1` - **IF** no entry carries the label, or the call fails, times out or answers neither 2xx nor 404: **IF** the message's delivery attempt count reached `cleanup_worker.max_attempts`, log the attachment id with the sanitized cause, record `mini_chat_cleanup_failed{resource_type="file"}` and **RETURN** `Reject` (terminal provider-file debt, dead-lettered for replay); **ELSE** record `mini_chat_cleanup_retry{resource_type="file",reason="provider_error"}` and **RETURN** `Retry` - `inst-orphan-if-failed`
    3. [x] - `p1` - **IF** the payload has a `secondary_ref`, delete it as in step 8 (best effort, never blocks the outcome) - `inst-orphan-delete-secondary`
    4. [x] - `p1` - Record `mini_chat_cleanup_completed{resource_type="file"}` and **RETURN** `Ok` (2xx and 404 alike, so a redelivery is harmless) - `inst-orphan-return-ok`

### Abandoned Upload Reaper

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-reap-abandoned`

**Input**: `upload_reaper` (`enabled`, `scan_interval_secs`, `stale_after_secs`), the leader elector, the gear stop token, the current time

**Output**: Abandoned rows moved to `failed` / `upload_abandoned`, with cleanup messages for those with a provider file

**Steps**:
1. [x] - `p1` - At `start`, **IF** `upload_reaper.enabled` is `true`, spawn the reaper as a leader-only worker on the gear's background task registry (see `cpt-cf-mini-chat-flow-gear-foundation-gear-start`, `inst-spawn-workers`); it ends when the stop token is cancelled - `inst-spawn-reaper`
2. [x] - `p1` - **WHILE** the stop token is not cancelled, wait `scan_interval_secs`, then **IF** the elector reports leadership for the Lease `mini-chat-upload-reaper`, run one scan - `inst-scan-loop`
3. [x] - `p1` - Set the cutoff to now minus `stale_after_secs` (application UTC clock) and start the scan timer - `inst-cutoff`
4. [x] - `p1` - DB: SELECT at most 100 `attachments` (system read through the `idx_attachments_stale_upload` index) WHERE `status` is `pending` or `uploaded`, `deleted_at IS NULL`, `cleanup_status IS NULL` and `updated_at` is before the cutoff, ordered by `updated_at` ascending; later scans pick up the rest - `inst-select-stale`
5. [x] - `p1` - **FOR EACH** selected row - `inst-for-each-stale`
   1. [x] - `p1` - Open a transaction write-first: DB: UPDATE `attachments` SET `status` `failed`, `error_code` `upload_abandoned`, `updated_at` now and, when the row has a `provider_file_id`, `cleanup_status` `pending` and `cleanup_updated_at` now, WHERE `id` and `tenant_id` match, `status` is the selected status, `deleted_at IS NULL`, `cleanup_status IS NULL` and `updated_at` is before the cutoff - `inst-cas-abandon`
   2. [x] - `p1` - **IF** no row was updated (the upload finished, or the row was deleted or claimed by chat cleanup), roll back and skip the row - `inst-if-abandon-lost`
   3. [x] - `p1` - **IF** the row has a `provider_file_id`, enqueue the cleanup message with `event_type` `attachment_upload_abandoned` and `secondary_ref` `null` in the same transaction; **IF** the row has a `secondary_file_id`, log a warning naming it (that copy is not deleted) - `inst-enqueue-abandoned`
   4. [x] - `p1` - Commit; then record `mini_chat_attachment_upload_abandoned{from_status}` with the selected status; a failed row transaction is logged and the scan continues with the next row - `inst-commit-abandon`
6. [x] - `p1` - Record `mini_chat_upload_reaper_scan_duration_seconds`; the row is never soft-deleted, stays visible through `GET` as `failed`, and no longer counts toward the per-chat limits - `inst-scan-done`

### Attachment Metrics Recording

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-attachments-record-metrics`

**Input**: Upload outcomes, background indexing outcomes, reaper and cleanup events

**Output**: Recorded instruments with bounded labels only

**Steps**:
1. [x] - `p1` - Record `mini_chat_attachment_upload{kind,result}` once per upload request that passed classification: `result` `ok` for a 201 (`ready` or `uploaded`), `rejected` for a 4xx or 429 decided by this feature, `failed` for a 5xx or a 503 after the row was inserted - `inst-count-upload`
2. [x] - `p1` - Record `mini_chat_attachment_upload_bytes{kind}` with `size_bytes` after a committed insert - `inst-upload-bytes`
3. [x] - `p1` - Add 1 to `mini_chat_attachments_pending` after the insert commits and subtract 1 when the request moves the row out of `pending` or ends - `inst-pending-gauge`
4. [x] - `p1` - Record `mini_chat_attachment_background_indexing{result}` (`ready`, `failed`, `timeout`, `set_ready_failed`) once per background task that reached an outcome - `inst-background-metric`
5. [x] - `p1` - Record `mini_chat_attachment_upload_abandoned{from_status}` after the commit and `mini_chat_upload_reaper_scan_duration_seconds` per scan - `inst-reaper-metrics`
6. [x] - `p1` - Record `mini_chat_cleanup_completed`, `mini_chat_cleanup_failed`, `mini_chat_cleanup_retry` with `resource_type` `file` (and `reason` `provider_error`) and `mini_chat_secondary_cleanup_skipped{provider_kind}` from the cleanup handler and the delete flow - `inst-cleanup-metrics`
7. [x] - `p1` - Never use a tenant, user, chat, attachment, file or vector store id, a filename or a MIME type as a label - `inst-no-high-cardinality`

## 4. States (CDSL)

### Attachment Status State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-attachment-status`

**States**: pending, uploaded, ready, failed

**Initial State**: pending

**Transitions**:
1. [x] - `p1` - **FROM** pending **TO** uploaded **WHEN** the provider returned a file id and the CAS on `pending`, `deleted_at IS NULL` and `cleanup_status IS NULL` stored it - `inst-to-uploaded`
2. [x] - `p1` - **FROM** uploaded **TO** ready **WHEN** a code-interpreter-only document or an image finished its request steps, or a `for_file_search` document reported `completed` in the request or in the background task, through `cpt-cf-mini-chat-algo-attachments-mark-ready` - `inst-to-ready`
3. [x] - `p1` - **FROM** pending **TO** failed **WHEN** the provider upload failed (`upload_failed`, no provider file, `cleanup_status` stays NULL), the returned provider file id could not be stored by the `uploaded` CAS because of a database error (`upload_failed`, the id written with `cleanup_status` `pending` and the cleanup message in one transaction), or the reaper found the row stale (`upload_abandoned`) - `inst-pending-to-failed`
4. [x] - `p1` - **FROM** uploaded **TO** failed **WHEN** the vector store could not be resolved (`vector_store_failed`), indexing failed or timed out in the request or the background task (`indexing_failed`), the `ready` write failed with a database error (`upload_failed`), or the reaper found the row stale (`upload_abandoned`); every one of these sets `cleanup_status` `pending` and enqueues the cleanup message in the same transaction, because an `uploaded` row always has a provider file - `inst-uploaded-to-failed`

`ready` and `failed` are terminal for `status`. Soft delete is orthogonal to it: `deleted_at` is set in any state, `status` keeps its value, and the row leaves every read and every later transition, because each CAS above requires `deleted_at IS NULL`.

### Attachment Cleanup State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-cleanup-status`

**States**: none, pending, done, failed

**Initial State**: none

**Transitions**:
1. [x] - `p1` - **FROM** none **TO** pending **WHEN** the attachment is deleted by the user, its chat is soft-deleted (`cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`), the reaper fails a stale row that has a provider file, the background task fails indexing, or the upload request fails a row that has a provider file (`upload_failed`, `vector_store_failed`, `indexing_failed`, through `cpt-cf-mini-chat-algo-attachments-fail-upload`); every case except the chat delete enqueues one attachment cleanup message in the same transaction. An `attachment_orphan_file` message never changes `cleanup_status`: its file id is on no row, and the handler bounds its retries by the delivery attempt count - `inst-to-pending`
2. [x] - `p1` - **FROM** pending **TO** done **WHEN** the attachment cleanup handler (or the chat-cleanup handler for a deleted chat) deleted the provider file with 2xx or 404, or the payload had no provider file - `inst-to-done`
3. [x] - `p1` - **FROM** pending **TO** failed **WHEN** `cleanup_attempts` reached `cleanup_worker.max_attempts`; the message is rejected and the file is unresolved provider debt - `inst-to-failed`

### Secondary Copy State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-secondary-copy`

**States**: not_attempted, pending, uploaded, failed

**Initial State**: not_attempted

**Transitions**:
1. [x] - `p1` - **FROM** not_attempted **TO** pending **WHEN** an image of an Anthropic chat within `thumbnail.max_decode_bytes` starts its secondary upload - `inst-secondary-to-pending`
2. [x] - `p1` - **FROM** pending **TO** uploaded **WHEN** the Anthropic Files API returned a file id - `inst-secondary-to-uploaded`
3. [x] - `p1` - **FROM** pending **TO** failed **WHEN** the secondary upload failed or timed out; the attachment can still become `ready` - `inst-secondary-to-failed`

### Chat Vector Store State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-attachments-chat-vector-store`

**States**: absent, creating, bound

**Initial State**: absent

**Transitions**:
1. [x] - `p1` - **FROM** absent **TO** creating **WHEN** the first `for_file_search` upload of the chat wins the placeholder INSERT under UNIQUE `(tenant_id, chat_id)` - `inst-to-creating`
2. [x] - `p1` - **FROM** creating **TO** bound **WHEN** the provider created the store and the CAS on `vector_store_id IS NULL` stored its id - `inst-to-bound`
3. [x] - `p1` - **FROM** creating **TO** absent **WHEN** the provider create failed (best-effort placeholder delete) or a later upload reclaimed a placeholder older than 120 s - `inst-creating-to-absent`
4. [x] - `p1` - **FROM** bound **TO** absent **WHEN** the chat-cleanup handler deleted the provider store of a soft-deleted chat (`cpt-cf-mini-chat-feature-background-workers`); this feature never deletes a bound row - `inst-bound-to-absent`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Upload Endpoint and Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-upload-endpoint`

The system **MUST** serve `POST /v1/chats/{id}/attachments` as specified in `cpt-cf-mini-chat-flow-attachments-upload-attachment`, in the §1.4 check order: PEP `upload_attachment`, chat load, chat-model resolution without the enabled filter (`INVALID_MODEL` before the body is read, `cpt-cf-mini-chat-constraint-model-locked-per-chat`), a non-waiting permit of `rag.max_concurrent_uploads` (503 `Retry-After: 5`), multipart reading of the `file` part, MIME normalization against the §1.4 allowlist with extension inference and CSV remapping, kind and purpose routing, the `disable_images` and code-interpreter rules, the per-kind size limits lowered by `max_file_size_mb`, the `provider_mismatch` pre-check, and the write-first pending insert that enforces `rag.max_documents_per_chat` and `rag.max_total_upload_mb_per_chat` (429 `document_limit` / `storage_limit`). The response is 201 with the `AttachmentDetail` in `ready`, or in `uploaded` when indexing is still running at the deadline.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-upload-attachment`
- `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`
- `cpt-cf-mini-chat-algo-attachments-read-multipart`
- `cpt-cf-mini-chat-algo-attachments-classify-file`
- `cpt-cf-mini-chat-algo-attachments-read-file-content`
- `cpt-cf-mini-chat-algo-attachments-insert-pending`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`, `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- API: `POST /v1/chats/{id}/attachments`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores` (read)
- Design: `cpt-cf-mini-chat-design-rag-quality-controls`, `cpt-cf-mini-chat-design-code-interpreter`, `cpt-cf-mini-chat-design-model-catalog`
- Config: `rag.*`, `thumbnail.max_decode_bytes`
- Entities: Attachment

### Provider File Storage Through OAGW

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-provider-storage`

The system **MUST** store every accepted file with the RAG provider through the OAGW in-process proxy as specified in `cpt-cf-mini-chat-algo-attachments-upload-provider-file` and the §1.4 "Storage wire contract": `POST /{alias}/v1/files` (OpenAI) or `POST /{alias}/openai/files?api-version=...` (Azure), `multipart/form-data` with `purpose` `assistants` for documents and images alike and the file named `{chat_id}_{attachment_id}.{ext}`. The storage provider is chosen by the chat model's provider entry and its `rag_provider` (`cpt-cf-mini-chat-constraint-openai-compatible`, `cpt-cf-mini-chat-component-llm-provider`, `cpt-cf-mini-chat-adr-llm-provider-as-library`). The returned id is stored only in `attachments.provider_file_id` through the `pending` to `uploaded` CAS; a lost CAS enqueues the row-less `attachment_orphan_file` cleanup message for the new file. Provider writes are never retried, carry no credentials and are bounded by the request deadline; failures leave the row `failed` with `upload_failed` and answer 503 `Retry-After: 10`. Every failure of a row that has a provider file, whatever its error code and including a database error on the `uploaded` or `ready` write, goes through `cpt-cf-mini-chat-algo-attachments-fail-upload`: one write-first transaction writes `failed`, the `provider_file_id`, `cleanup_status` `pending` and enqueues the `attachment_upload_failed` or `attachment_indexing_failed` message with the delete payload, so the cleanup handler deletes the file with retries. Only the residual risks of §1.4 (orphan vector store, database unavailable) keep a best-effort delete.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-build-storage-request`
- `cpt-cf-mini-chat-algo-attachments-upload-provider-file`
- `cpt-cf-mini-chat-algo-attachments-fail-upload`
- `cpt-cf-mini-chat-state-attachments-attachment-status`

**Constraints**: `cpt-cf-mini-chat-constraint-openai-compatible`, `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- External: OAGW proxy (`ServiceGatewayClientV1`), provider Files API
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`
- Components: `cpt-cf-mini-chat-component-llm-provider`, `cpt-cf-mini-chat-component-chat-service`

### Chat Vector Store and Indexing

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-vector-store-indexing`

The system **MUST** keep exactly one provider vector store per chat (`cpt-cf-mini-chat-design-vector-store-scope`, `cpt-cf-mini-chat-nfr-rag-scalability`) through the placeholder protocol of `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`: the auto-committed placeholder INSERT under UNIQUE `(tenant_id, chat_id)`, a single provider create by the winner, the CAS on `vector_store_id IS NULL`, loser polling (503 after 5 polls), reclaim of placeholders older than 120 s, and `provider_mismatch` on a store of another backend. It **MUST** add every `for_file_search` document with the single attribute `attachment_id`. It **MUST** wait for `completed` until the upload start plus 25 s as in `cpt-cf-mini-chat-algo-attachments-index-document`, answering `ready`, 503 with `indexing_failed` (the row `failed` with `cleanup_status` `pending` and the `attachment_indexing_failed` cleanup message in one transaction, no fire-and-forget delete), or 201 `uploaded`. A `vector_store_failed` row with a provider file is handled the same way with `attachment_upload_failed`; only an orphan provider store of a lost CAS is deleted best effort (§1.4 residual risk). The background continuation of `cpt-cf-mini-chat-algo-attachments-background-indexing` (20 s heartbeat rounds up to 10 minutes, stop on deletion, chat deletion or gear stop) moves the row to `ready`, or to `failed` / `indexing_failed` with `cleanup_status` `pending` and the `attachment_indexing_failed` cleanup message in one transaction.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-ensure-vector-store`
- `cpt-cf-mini-chat-algo-attachments-index-document`
- `cpt-cf-mini-chat-algo-attachments-background-indexing`
- `cpt-cf-mini-chat-algo-attachments-mark-ready`
- `cpt-cf-mini-chat-state-attachments-chat-vector-store`

**Touches**:
- External: provider Vector Stores API through OAGW
- DB Table: `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-attachments`
- DB: shared outbox tables (queue `mini-chat.attachment_cleanup`)
- Sequence: `cpt-cf-mini-chat-seq-file-upload`
- Entities: ChatVectorStore, Attachment

### Image Thumbnails and Secondary Copies

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-images`

The system **MUST** generate image thumbnails as specified in `cpt-cf-mini-chat-algo-attachments-generate-thumbnail`: WebP, fit inside `thumbnail.width` × `thumbnail.height`, at most `thumbnail.max_bytes`, skipped by the `max_decode_bytes` and `max_pixels` screens and the capped decoder, stored only in `attachments.img_thumbnail` with its width and height, and never failing the upload or setting `error_code`. For a chat whose model's provider is `anthropic_messages`, it **MUST** upload a secondary copy of each image within `thumbnail.max_decode_bytes` to the Anthropic Files API as in `cpt-cf-mini-chat-algo-attachments-upload-secondary-copy`, tracking `secondary_status` and `secondary_file_id` and tolerating its failure. Images are never added to a vector store, and their kind is recorded for the per-turn vision check of the send pipeline (`cpt-cf-mini-chat-constraint-model-image-capability`).

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-generate-thumbnail`
- `cpt-cf-mini-chat-algo-attachments-upload-secondary-copy`
- `cpt-cf-mini-chat-state-attachments-secondary-copy`

**Constraints**: `cpt-cf-mini-chat-constraint-model-image-capability`

**Touches**:
- External: Anthropic Files API through OAGW
- DB Table: `cpt-cf-mini-chat-dbtable-attachments` (thumbnail and secondary columns)
- Config: `thumbnail.*`

### Get Attachment API

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-get-attachment`

The system **MUST** serve `GET /v1/chats/{id}/attachments/{attachment_id}` as specified in `cpt-cf-mini-chat-flow-attachments-get-attachment`: PEP `read_attachment` on the chat, the owner-scoped chat, the live attachment of that chat uploaded by the caller, and the `AttachmentDetail` of `cpt-cf-mini-chat-algo-attachments-project-detail` with `img_thumbnail` only for ready images, `error_code` only for failed rows, `doc_summary` and `summary_updated_at` never present (`cpt-cf-mini-chat-fr-doc-summary`), and no provider identifier (`cpt-cf-mini-chat-principle-owner-only-content`).

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-get-attachment`
- `cpt-cf-mini-chat-algo-attachments-project-detail`

**Touches**:
- API: `GET /v1/chats/{id}/attachments/{attachment_id}`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments` (read)

### Attachment Deletion

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-delete`

The system **MUST** serve `DELETE /v1/chats/{id}/attachments/{attachment_id}` as specified in `cpt-cf-mini-chat-flow-attachments-delete-attachment` and `cpt-cf-mini-chat-algo-attachments-soft-delete` (`cpt-cf-mini-chat-fr-attachment-deletion`). It checks the uploader before idempotency and answers 204 for a repeated delete without a new message. It answers 409 `attachment_locked` when any `message_attachments` row references the attachment. Otherwise one write-first transaction soft-deletes the row, sets `cleanup_status` `pending` when it had none, and enqueues the `attachment_deleted` payload of `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload` to `mini-chat.attachment_cleanup` partitioned by `tenant_id`; the response is 204 once it commits. The deleted attachment leaves chat metadata, the `file_search` inclusion check and citations at once, and its chunks stay searchable until the provider file is deleted (`cpt-cf-mini-chat-adr-document-retrieval-scope`).

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-delete-attachment`
- `cpt-cf-mini-chat-algo-attachments-soft-delete`
- `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`

**Touches**:
- API: `DELETE /v1/chats/{id}/attachments/{attachment_id}`
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-message-attachments` (read)
- DB: shared outbox tables (queue `mini-chat.attachment_cleanup`)

### Attachment Cleanup Outbox Handler

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-cleanup-handler`

The system **MUST** replace the foundation placeholder of `outbox.cleanup_queue_name` with the handler of `cpt-cf-mini-chat-algo-attachments-handle-cleanup`, registered with the outbox default lease. The handler rejects malformed payloads and acknowledges messages of soft-deleted chats without action. It deletes the primary provider file through the `storage_backend` provider entry (2xx and 404 are success) and the `secondary_ref` copy best effort, with no Vector Stores API call. It moves `cleanup_status` from `pending` to `done`, or counts the failed attempt and returns `Retry`, and after `cleanup_worker.max_attempts` attempts sets `failed` and returns `Reject` (`cpt-cf-mini-chat-state-attachments-cleanup-status`). A database failure returns `Retry` without counting. It handles the `attachment_upload_failed` and in-request `attachment_indexing_failed` messages of `cpt-cf-mini-chat-algo-attachments-fail-upload` like the other row-bearing events. It **MUST** also accept the row-less `attachment_orphan_file` variant: it deletes the payload's provider file (and `secondary_ref`) without reading or writing any row, also when the row is gone, not `pending` or in a soft-deleted chat, returns `Ok` on 2xx or 404, and returns `Retry` on a failed delete until the outbox delivery attempt count reaches `cleanup_worker.max_attempts`, then `Reject`. No gear-local polling or claim worker exists for this path.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-handle-cleanup`
- `cpt-cf-mini-chat-algo-attachments-fail-upload`
- `cpt-cf-mini-chat-state-attachments-cleanup-status`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-attachments` (cleanup columns)
- DB: shared outbox tables (queue `mini-chat.attachment_cleanup`)
- External: provider Files API and Anthropic Files API through OAGW

### Upload Reaper Worker

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-upload-reaper`

The system **MUST** run the abandoned-upload reaper of `cpt-cf-mini-chat-algo-attachments-reap-abandoned` when `upload_reaper.enabled` is `true`, every `upload_reaper.scan_interval_secs` while the elector reports leadership for the Lease `mini-chat-upload-reaper`. Each scan takes at most 100 stale `pending` or `uploaded` rows without a cleanup status, ordered by `updated_at`. Each row moves in its own CAS transaction to `failed` / `upload_abandoned`, and a row with a provider file gets `cleanup_status` `pending` and the `attachment_upload_abandoned` message in the same transaction. Rows owned by chat cleanup are skipped, rows are never soft-deleted, and the worker stops with the gear.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-reap-abandoned`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-attachments` (index `idx_attachments_stale_upload`)
- DB: shared outbox tables (queue `mini-chat.attachment_cleanup`)
- Config: `upload_reaper.*`

### Send Pipeline Readiness and Citation Data

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-send-pipeline-data`

The system **MUST** keep the attachment data that the send pipeline reads consistent with `cpt-cf-mini-chat-algo-attachments-mark-ready`. `status` `ready` is written only there and implies a stored `provider_file_id`. A ready `for_file_search` document implies a bound `chat_vector_stores` row whose store holds the indexed file (the first such document turns `file_search` on, `cpt-cf-mini-chat-fr-file-search`). A ready `for_code_interpreter` file has the provider file id passed as a container file (`cpt-cf-mini-chat-fr-code-interpreter`). A ready image has its `provider_file_id` and, in Anthropic chats, its `secondary_file_id` when the copy succeeded. `provider_file_id` holds exactly the id the provider returns in `file_citation.file_id`, so the in-memory map `provider_file_id` to (`attachment_id`, `filename`) of `cpt-cf-mini-chat-algo-streaming-turns-map-citations` resolves file citations within the chat (`cpt-cf-mini-chat-design-rag-quality-controls`). Non-ready and deleted rows are rejected by `cpt-cf-mini-chat-algo-streaming-turns-create-turn` as `invalid_attachment`. `doc_summary`, `summary_model` and `summary_updated_at` are never written (`cpt-cf-mini-chat-fr-doc-summary`).

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-mark-ready`
- `cpt-cf-mini-chat-state-attachments-attachment-status`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-chat-vector-stores`, `cpt-cf-mini-chat-dbtable-message-attachments`
- Consumers: `cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`, `cpt-cf-mini-chat-algo-streaming-turns-decide-tools`, `cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`

### Attachment Error Contract

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-error-contract`

The system **MUST** report every failure of the three routes as the canonical `Problem` of `cpt-cf-mini-chat-algo-gear-foundation-map-error` (`cpt-cf-mini-chat-adr-canonical-error-contract`) with the reasons of the §1.4 REST contract. It **MUST** apply the two corrections of the §1.4 deviation table in the foundation mapping: the multipart field per reason (`content_type` for `BOUNDARY_REQUIRED` and `MISSING_CONTENT_TYPE`, `multipart` for `MULTIPART_ERROR`, `file` for `MISSING_FILE`), and the per-chat limit violation `{ subject, description }` equal to `document_limit` or `storage_limit`. A 404 names the chat type for the chat and the attachment type for the attachment. Storage failures are 503 with `Retry-After: 10` and the concurrency limit 503 with `Retry-After: 5`, both with `context.retry_after_seconds`. No `detail` carries a provider identifier, provider text or `error_code`.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-fail-upload`
- `cpt-cf-mini-chat-algo-attachments-read-multipart`
- `cpt-cf-mini-chat-algo-attachments-insert-pending`

**Touches**:
- API: the three routes of this feature
- Components: the foundation error mapping (`api/rest/error_mapping.rs`)

### Attachment Metrics

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-metrics`

The system **MUST** record the upload, background indexing, reaper and file-cleanup instruments of `cpt-cf-mini-chat-algo-attachments-record-metrics` with bounded labels only (`cpt-cf-mini-chat-nfr-observability-supportability`), and log failures with attachment and chat ids but never file content, provider response text in a response, or credentials.

**Implements**:
- `cpt-cf-mini-chat-algo-attachments-record-metrics`

**Touches**:
- Observability: `mini_chat_*` meter (instruments registered by `cpt-cf-mini-chat-algo-gear-foundation-register-metrics`)

### REST Registration and OpenAPI

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-rest-routes`

The system **MUST** register, under `url_prefix` and with the base license feature (`cpt-cf-mini-chat-interface-public-api`), `POST {prefix}/v1/chats/{id}/attachments` as operation `mini_chat.upload_attachment` with a `multipart/form-data` body (`file`, binary, required) and no route body limit, response 201 `AttachmentDetailDto` and the 400, 401, 403, 404, 409, 429, 500 and 503 (`Retry-After`) problem responses. It **MUST** register `GET {prefix}/v1/chats/{id}/attachments/{attachment_id}` as `mini_chat.get_attachment`, response 200 `AttachmentDetailDto` and the 400, 401, 403, 404, 500 and 503 problem responses. It **MUST** register `DELETE` on the same path as `mini_chat.delete_attachment`, response 204 and the 400, 401, 403, 404, 409, 500 and 503 problem responses. All three use tag `Mini Chat Attachments`. The attachment service, the storage client, the cleanup handler and the reaper are added to the shared application services and the gear lifecycle.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-upload-attachment`
- `cpt-cf-mini-chat-flow-attachments-get-attachment`
- `cpt-cf-mini-chat-flow-attachments-delete-attachment`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `POST /v1/chats/{id}/attachments`, `GET /v1/chats/{id}/attachments/{attachment_id}`, `DELETE /v1/chats/{id}/attachments/{attachment_id}`

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-attachments-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: fail-closed authorization of the three actions (deny 403 `AUTHZ_DENIED`, PDP failure 503 `Retry-After: 5`); every attachment read and write bound to the owner-scoped chat and filtered by `tenant_id`, `chat_vector_stores` never read without `tenant_id` and `chat_id` (`cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-principle-tenant-scoped`); another uploader's row is a 404; no provider file id, vector store id, secondary id, provider text or credential in any response, metric label or log record meant for users; credentials only injected by OAGW; thumbnail decoding bounded by `thumbnail.max_decode_bytes` against pixel bombs; uploaded content never logged.
- **Reliability**: every external call bounded as in the §1.4 timeout table; the request answers within the 25 s deadline; a dropped request or a stopped gear leaves rows that the reaper finishes; every cleanup message is retried by the shared outbox until `done`, terminal `failed` or dead-lettered; every provider file id an upload obtains is recorded durably before the request answers (on the row with `cleanup_status` `pending` and a cleanup message, or in an `attachment_orphan_file` message), except in the two §1.4 residual risks (orphan vector store, database unavailable); best-effort deletes never block a response.
- **Data integrity**: the transaction discipline of §1.4 (write-first or `IMMEDIATE` on SQLite, no provider call inside a transaction, deterministic closures, bounded conflict retry); one vector store per chat by UNIQUE `(tenant_id, chat_id)` and the CAS; status changes only through the CAS updates of `cpt-cf-mini-chat-state-attachments-attachment-status`; a locked attachment is never deleted; per-chat limits exact on SQLite, with the PostgreSQL residual risk recorded in §1.4.
- **Performance**: content memory bounded by `rag.max_concurrent_uploads` × the size limit; thumbnails on blocking threads; the per-chat limit queries and the reaper scan use the `(tenant_id, chat_id)` and `idx_attachments_stale_upload` indexes.
- **Observability**: the instruments of `cpt-cf-mini-chat-algo-attachments-record-metrics`; warnings for secondary-copy failures, skipped secondary cleanups and the first transient indexing error; the last transient error in a background timeout log.
- **Rollback**: the feature adds no migration; removing it removes the three routes, the reaper and the handler (the foundation placeholder then holds cleanup messages with `Retry`, losing none); existing rows stay valid and readable by the send pipeline.
- **Test layering**: the feature is testable without a real provider. Unit tests cover multipart reading (each reason and field), the MIME table with aliases, extension inference and CSV, purpose filtering, size limits with `max_file_size_mb`, thumbnail sizing and screening, payload encoding and decoding, and the projection. Service-level tests run on SQLite and PostgreSQL with a fake `ServiceGatewayClientV1` that scripts Files, Vector Stores and Anthropic responses, delays, 5xx, 429 and gateway timeouts, a mock PDP and the static policy plugin. They check rows, outbox messages and CAS races: two concurrent first uploads create one store, deletes race with uploads, reaper races with the background task. A repository wrapper injects a database error at a chosen statement (the `uploaded` CAS, the `ready` CAS, the failure transaction) to verify the durable cleanup of `cpt-cf-mini-chat-algo-attachments-fail-upload`. End-to-end tests drive the REST routes with an OpenAI-compatible mock behind a real OAGW upstream provisioned by the gear, asserting the exact `files` and `vector_stores` requests. No live provider is called.
- **Compile-time gates**: domain code depends on ports (attachment repository, storage client, outbox enqueuer, elector), not on SeaORM entities or the OAGW SDK types; the heartbeat interval is a compile-time-checked constant at most half the minimum `stale_after_secs`; `make dylint` passes.
- **Completion signals**: automated tests exist and pass for every acceptance item of §6; `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass; every implemented CDSL instruction of this feature is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability) and every flow, algorithm, state and definition-of-done ID of this feature appears in code; the checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-flow-attachments-upload-attachment`
- `cpt-cf-mini-chat-algo-attachments-insert-pending`
- `cpt-cf-mini-chat-algo-attachments-soft-delete`

**Constraints**: `cpt-cf-mini-chat-constraint-no-credentials`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Attachments — "Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation" (document): a multipart upload of `report.pdf` (`application/pdf`) to an own chat makes the mock receive `POST /{alias}/v1/files` with parts `purpose` = `assistants` and `file` named `{chat_id}_{attachment_id}.pdf`, then `POST /{alias}/v1/vector_stores` once, then `POST /{alias}/v1/vector_stores/{vs}/files` with `file_id` and `attributes.attachment_id`. The response is 201 with exactly `id`, `filename` `report.pdf`, `content_type` `application/pdf`, `size_bytes`, `status` `ready`, `kind` `document`, `created_at`, and no `error_code`, `img_thumbnail`, `doc_summary`, `summary_updated_at` or provider id. The `attachments` row has `provider_file_id` the mock id, `for_file_search` `true`, `storage_backend` the label and `uploaded_by_user_id` the caller; one `chat_vector_stores` row exists with the mock store id. A second document reuses the store without a create call, and `GET` returns the same detail.
- [ ] Attachments — upload lifecycle (images and spreadsheets): a 300 × 150 PNG uploads with `purpose` `assistants`, makes no vector store call, and returns 201 `ready` with `kind` `image` and `img_thumbnail` `{ content_type: image/webp, width: 128, height: 64, data_base64 }` whose decoded bytes are a WebP of at most `thumbnail.max_bytes`. A corrupt PNG still returns 201 `ready` without `img_thumbnail` and without `error_code`. In a chat whose model uses an `anthropic_messages` provider, the mock also receives `POST /{anthropic_alias}/v1/files` without `purpose`, and the row has `secondary_status` `uploaded` and `secondary_file_id`; a failing secondary call gives `secondary_status` `failed` and still 201 `ready`. An XLSX upload with code interpreter available returns 201 `ready` with `for_code_interpreter` `true` and no vector store call.
- [ ] Attachments — upload lifecycle (validation): no boundary gives 400 `BOUNDARY_REQUIRED` with field `content_type`; a truncated body gives `MULTIPART_ERROR` with field `multipart`; no `file` part gives `MISSING_FILE` with field `file`; a `file` part without `Content-Type` gives `MISSING_CONTENT_TYPE` with field `content_type`. `application/zip`, and `application/octet-stream` with an unknown extension, give 400 `UNSUPPORTED_CONTENT_TYPE`, while `application/octet-stream` with `.docx` is accepted as the DOCX type. `text/csv` is stored as `text/plain`, or rejected when `rag.allow_csv_upload` is `false`. A document over `rag.uploaded_file_max_size_kb`, an image over `rag.uploaded_image_max_size_kb`, and a file over a smaller catalog `max_file_size_mb` give 400 `out_of_range` `FILE_TOO_LARGE` with field `content_length`. An image under `disable_images` gives 400 `failed_precondition` `FEATURE_DISABLED` `images`. An XLSX under `disable_code_interpreter`, or on a model without `tool_support.code_interpreter`, gives 400 `CODE_INTERPRETER_UNAVAILABLE`. A chat whose model left the catalog gives 400 `INVALID_MODEL` before the body is read. An unknown or foreign chat gives 404 with the chat type. In every case the mock receives no request and no `attachments` row exists.
- [ ] Attachments — upload lifecycle (per-chat limits): with `rag.max_documents_per_chat` 2, a third live document gives 429 `resource_exhausted` with `violations[0]` `{ subject: document_limit, description: document_limit }`, while an image is still accepted. A `failed` or deleted document frees its slot. With `rag.max_total_upload_mb_per_chat` 1, a 600 KiB file after another 600 KiB file gives 429 `storage_limit`, counting images too. In each rejected case no row exists and the mock receives no request.
- [ ] Attachments — upload lifecycle (get and delete): `GET` of a `failed` row returns `status` `failed` and its `error_code`. `GET` and `DELETE` of an unknown id, a row of another chat or a row uploaded by another user return 404 with `gts.cf.core.mini_chat.attachment.v1~`. `DELETE` of an unreferenced attachment returns 204, sets `deleted_at` and `cleanup_status` `pending`, and writes exactly one `mini-chat.attachment_cleanup` message with the ten payload keys, `event_type` `attachment_deleted` and `vector_store_id` `null`. The row then disappears from `GET` (404) and from the message list. A second `DELETE` returns 204 without a second message. `DELETE` of an attachment referenced by a sent message returns 409 `already_exists` with `resource_name` `attachment_locked` and leaves the row unchanged.
- [ ] Attachments — "Asynchronous indexing lifecycle, including provider failure and timeout handling": a mock that answers the add-file call with `in_progress` and `completed` on the second status read gives 201 `ready`. A mock that stays `in_progress` past 25 s gives 201 `uploaded` within 30 s of the request start; a later status read of `completed` makes `GET` return `ready` and the `updated_at` heartbeat advances every 20 s meanwhile. A status of `failed` (or `cancelled`, or an unknown value, or a 404 status read) within the deadline gives 503 with `Retry-After: 10` and a body without `indexing_failed`; the row is `failed` / `indexing_failed` with `cleanup_status` `pending` and exactly one `attachment_indexing_failed` message written in the same commit, no provider delete is sent by the request, and running the cleanup handler makes the mock receive one `DELETE /v1/files/{id}` and sets `cleanup_status` `done`. A failing store create or a `provider_mismatch` race after the provider file was stored gives the row `failed` / `vector_store_failed` with `cleanup_status` `pending` and one `attachment_upload_failed` message. Provider 5xx on status reads keep polling. A failing `POST files` gives 503 with the row `failed` / `upload_failed`. A failing store create gives 503 with `vector_store_failed` and no placeholder row left. In the background task, `failed` or the 10-minute limit (shortened in tests) give `failed` / `indexing_failed`, `cleanup_status` `pending` and one `attachment_indexing_failed` message in the same commit, and the task stops without changes when the row or chat is deleted or the gear stops. A message referencing an `uploaded` attachment gets 400 `invalid_attachment`.
- [ ] Attachments — "Attachments are correctly made available to the relevant provider tools" (attachment data part): after a ready document, a send carries `file_search` with the chat's `vector_store_id` from `chat_vector_stores`. After a ready XLSX, a send carries `code_interpreter` with its `provider_file_id`. A ready image in `attachment_ids` is sent as `input_image` with its `provider_file_id`, and in Anthropic chats as an image block with its `secondary_file_id`. A `file_citation` naming the document's `provider_file_id` yields a citation with its `attachment_id` and `filename`, and none after the attachment is deleted. A `pending`, `uploaded`, `failed` or deleted attachment never contributes a tool, image or citation.
- [ ] Attachments — "Cleanup and abandoned-upload recovery behave correctly under failure": the cleanup handler deletes the provider file of an `attachment_deleted` message and sets `cleanup_status` `done`. A mock 404 also counts as done. Mock 500s increase `cleanup_attempts` and return `Retry` until `cleanup_worker.max_attempts`, when `cleanup_status` becomes `failed` and the message is rejected. A message without `provider_file_id` becomes `done` without a provider call; a message for a soft-deleted chat is acknowledged without a call; a malformed payload is rejected; a `secondary_ref` triggers the Anthropic delete without blocking `done`. With `upload_reaper.stale_after_secs` 60, a `pending` row and an `uploaded` row whose `updated_at` is older than the cutoff become `failed` / `upload_abandoned`; only the `uploaded` one gets `cleanup_status` `pending` and an `attachment_upload_abandoned` message. Rows with a cleanup status, deleted rows and rows refreshed by a running background task are untouched, and the reaped rows stay visible through `GET`. An `attachment_orphan_file` message is processed without touching any row: its provider file is deleted (one `DELETE /v1/files/{id}`) and the handler returns `Ok` when the attachment row is missing, soft-deleted with `cleanup_status` `done`, `failed` / `upload_abandoned` with `cleanup_status` NULL, or in a soft-deleted chat; with mock 500s it returns `Retry` until the delivery attempt count reaches `cleanup_worker.max_attempts`, then `Reject`, and no row's `cleanup_attempts` changes.
- [ ] Durable cleanup at mark-ready (feature-local, finding A5-SRPT-001): with a database error injected on the `uploaded` to `ready` CAS of a PDF upload whose provider file is stored and indexed (and, separately, of an image whose Anthropic secondary copy is `uploaded`), the upload answers 500 `internal`; the row is `failed` / `upload_failed` with its `provider_file_id` and `cleanup_status` `pending`, and exactly one `mini-chat.attachment_cleanup` message exists with `event_type` `attachment_upload_failed`, that `provider_file_id` and the ten payload keys (for the image, a `secondary_ref` naming the secondary file id). Running the cleanup handler makes the mock receive `DELETE /{alias}/v1/files/{provider_file_id}` (and, for the image, the Anthropic secondary delete) and sets `cleanup_status` `done`; a mock 500 on that delete returns `Retry` and increases `cleanup_attempts` until the delete succeeds.
- [ ] Durable cleanup of lost and unrecorded provider files (feature-local, finding A5-SRPT-002): a database error injected on the `pending` to `uploaded` CAS gives 500 `internal`, the row `failed` / `upload_failed` with the returned `provider_file_id` and `cleanup_status` `pending`, and one `attachment_upload_failed` message; no provider delete is sent by the request. An attachment deleted (or its chat deleted, or the row reaped) while the mock holds `POST files` gives 404 and exactly one `attachment_orphan_file` message with the returned `provider_file_id`; the handler then deletes that file although the row is not `pending` or its chat is soft-deleted. When the failure transaction is also made to fail, a file id held only in memory gets one best-effort `DELETE` from the request, and a stored file id leaves the row `uploaded` for the reaper, which then enqueues its cleanup message. A lost vector store CAS sends one best-effort `DELETE /{alias}/v1/vector_stores/{vs}` and enqueues no message.
- [ ] Upload concurrency (feature-local): with `rag.max_concurrent_uploads` 1 and a mock that holds the first `POST files`, a second upload returns 503 with `Retry-After: 5` and `context.retry_after_seconds` 5 without any row or provider request; after the first completes, a new upload is accepted. A client that disconnects during an upload releases its permit.
- [ ] Vector store race (feature-local): two concurrent first document uploads into one chat (on SQLite and PostgreSQL) produce exactly one `POST /vector_stores` call, one `chat_vector_stores` row and two `ready` attachments in the same store. A placeholder row older than 120 s is reclaimed by the next upload. A chat whose store row has another `provider` gives 409 `provider_mismatch` for a document and still accepts an image.
- [ ] Tenant and owner isolation (attachment part): attachments of another tenant's or user's chat are never returned or deleted (404 with the chat type), also when the PDP returns only a tenant predicate; every `chat_vector_stores` query in tests carries `tenant_id` and `chat_id`; no response, SSE payload or metric label of this feature contains a `file-…`, `assistant-…` or `vs_…` id.
- [ ] Authorization (attachment operations): upload, get and delete evaluate `upload_attachment`, `read_attachment` and `delete_attachment` with the chat id as `resource.id`; PDP deny returns 403 `AUTHZ_DENIED` and PDP failure 503 with `Retry-After: 5`, both before any body read or row change.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature has `@cpt-begin` / `@cpt-end` markers.
