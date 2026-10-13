---
description: "Chat CRUD feature of the mini-chat gear: owner-scoped chat create, get, list, rename and soft delete with model and title validation and chat-cleanup outbox enqueue, the cursor-paginated message history API with its response contract, and like/dislike reactions on assistant messages."
---

# Feature: Chat CRUD, Messages and Reactions

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-chat-crud-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-mini-chat-feature-chat-crud`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Create Chat](#create-chat)
  - [Get Chat](#get-chat)
  - [List Chats](#list-chats)
  - [Rename Chat](#rename-chat)
  - [Delete Chat](#delete-chat)
  - [List Messages](#list-messages)
  - [Set Reaction](#set-reaction)
  - [Remove Reaction](#remove-reaction)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Title Validation](#title-validation)
  - [Chat Model Resolution](#chat-model-resolution)
  - [Chat Detail Projection](#chat-detail-projection)
  - [Cursor Pagination and OData Query](#cursor-pagination-and-odata-query)
  - [Chat Soft Delete and Cleanup Enqueue](#chat-soft-delete-and-cleanup-enqueue)
  - [Message Page Enrichment](#message-page-enrichment)
  - [Reaction Target Resolution](#reaction-target-resolution)
- [4. States (CDSL)](#4-states-cdsl)
  - [Chat Lifecycle State Machine](#chat-lifecycle-state-machine)
  - [Message Reaction State Machine](#message-reaction-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Chat Creation and Model Lock](#chat-creation-and-model-lock)
  - [Chat Read, List and Rename](#chat-read-list-and-rename)
  - [Soft Delete with Cleanup Enqueue](#soft-delete-with-cleanup-enqueue)
  - [Messages API](#messages-api)
  - [Reactions API](#reactions-api)
  - [Temporary Chats Deferred](#temporary-chats-deferred)
  - [REST Registration and OpenAPI](#rest-registration-and-openapi)
  - [Cross-Cutting Quality and Verification](#cross-cutting-quality-and-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

This feature delivers the owner-scoped chat management surface of the `mini-chat` gear on top of the foundation: chat create (with model validation and the deterministic default-model algorithm; the chat's model is immutable afterwards), chat get (metadata and `message_count`, no embedded messages), chat list (OData `$filter` / `$orderby`, cursor pagination, ordering by most recent activity), chat rename (title only), chat soft delete (204, sub-resources unreachable, chat-cleanup work enqueued on the shared outbox), the paginated message history API with its full response contract (`request_id`, `attachments`, `my_reaction`), and like/dislike reactions on assistant messages.

**Implementation slices** (implement in this order; each slice is independently testable):

| # | Slice | CDSL elements |
|---|-------|---------------|
| 1 | Validation and model selection | `cpt-cf-mini-chat-algo-chat-crud-validate-title`, `cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model` |
| 2 | Chat create, get and rename | `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`, `cpt-cf-mini-chat-flow-chat-crud-create-chat`, `cpt-cf-mini-chat-flow-chat-crud-get-chat`, `cpt-cf-mini-chat-flow-chat-crud-rename-chat` |
| 3 | Shared pagination and chat list | `cpt-cf-mini-chat-algo-chat-crud-paginate`, `cpt-cf-mini-chat-flow-chat-crud-list-chats` |
| 4 | Chat delete and cleanup enqueue | `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`, `cpt-cf-mini-chat-flow-chat-crud-delete-chat`, `cpt-cf-mini-chat-state-chat-crud-chat-lifecycle` |
| 5 | Messages API | `cpt-cf-mini-chat-algo-chat-crud-enrich-messages`, `cpt-cf-mini-chat-flow-chat-crud-list-messages` |
| 6 | Reactions API | `cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target`, `cpt-cf-mini-chat-flow-chat-crud-set-reaction`, `cpt-cf-mini-chat-flow-chat-crud-remove-reaction`, `cpt-cf-mini-chat-state-chat-crud-message-reaction` |

**Out of scope** (owned by other features): sending messages, turn persistence, `message_attachments` writes and the `chats.updated_at` bump on every sent message (`cpt-cf-mini-chat-feature-streaming-turns`); the bump on retry and edit (`cpt-cf-mini-chat-feature-turn-mutations`); attachment upload, get and delete (`cpt-cf-mini-chat-feature-attachments`); the chat-cleanup outbox handler body that deletes provider files and the chat vector store (`cpt-cf-mini-chat-feature-background-workers`); the temporary-chat toggle and 24h auto-deletion (P2, `cpt-cf-mini-chat-usecase-temporary-chat-cleanup`); hard-purge of soft-deleted rows and a chat-deletion audit event (not implemented, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`). The request gate, PEP, Secure ORM chat scoping, error mapping, schema and the five outbox queues come from `cpt-cf-mini-chat-feature-gear-foundation` and are referenced, not redefined.

### 1.2 Purpose

Users need to create conversations on a model of their choice, find and resume them, rename them, remove them, read their history page by page, and give feedback on answers. Every one of these operations must stay inside the owner's chats, reject malformed input with the canonical error contract, and never change the model a chat was created with. Deletion must answer immediately while the provider-side cleanup runs later through the durable outbox.

**Requirements**: `cpt-cf-mini-chat-fr-chat-crud`, `cpt-cf-mini-chat-fr-model-selection`, `cpt-cf-mini-chat-fr-conversation-history`, `cpt-cf-mini-chat-fr-message-reactions`, `cpt-cf-mini-chat-fr-chat-deletion-cleanup`, `cpt-cf-mini-chat-fr-temporary-chat`, `cpt-cf-mini-chat-nfr-tenant-isolation`, `cpt-cf-mini-chat-nfr-authz-alignment`

`cpt-cf-mini-chat-fr-chat-deletion-cleanup` is covered here only for its synchronous part (soft delete, 204, cleanup enqueue); the asynchronous provider cleanup belongs to `cpt-cf-mini-chat-feature-background-workers`. `cpt-cf-mini-chat-fr-temporary-chat` is traced only to confirm its P2 status: the reserved `is_temporary` column is never modified and no toggle endpoint exists.

**Principles**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Creates, reads, lists, renames and deletes own chats; lists messages of own chats; sets and removes reactions on assistant messages |
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | Not invoked by this feature; it consumes the chat-cleanup outbox message enqueued by chat delete (handler owned by `cpt-cf-mini-chat-feature-background-workers`) |
| ToolKit runtime (gear host) | Registers the eight routes of this feature under `url_prefix`; runs the api-gateway authentication and license middleware and the platform extractors; hosts the shared outbox that durably stores the chat-cleanup message |
| Model policy plugin (via the model-policy gateway) | Supplies the policy snapshot whose `model_catalog` drives model validation and the default-model algorithm on chat create |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-mini-chat-fr-chat-crud`, `cpt-cf-mini-chat-fr-model-selection`, `cpt-cf-mini-chat-fr-conversation-history`, `cpt-cf-mini-chat-fr-message-reactions`, `cpt-cf-mini-chat-fr-chat-deletion-cleanup`, `cpt-cf-mini-chat-fr-temporary-chat`, UC-004 `cpt-cf-mini-chat-usecase-delete-chat`
- **Design**: [DESIGN.md](../DESIGN.md) — §2.1, §2.2 "Model Locked Per Chat", §3.1, §3.2 (chat service, persistence layer, authz integration), §3.3 (Create / List / Get / Update / Delete Chat, List Messages, Message Reaction API, REST error mapping), §3.6 "Cleanup on Chat Deletion" (`cpt-cf-mini-chat-seq-chat-deletion-cleanup`, "Outbox payload and execution semantics"), §3.7 (`chats`, `messages`, `attachments`, `message_attachments`, `message_reactions`), §3.8 (per-operation authorization matrix, fail-closed behavior), §4 "P1 Scope Boundaries", "Model Catalog Configuration", "Cleanup on Chat Deletion", Appendix B.9.3, B.10
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) entry 2.2
- **Acceptance checklist**: [acceptance-criteria.md](../acceptance-criteria.md) — owned items: Principles & Constraints "A chat's model is immutable once set"; Chat CRUD (all three items); Messages API "List messages with filtering, ordering, and pagination" and "Message response contract: identity, attachments, and reaction fields are always consistently present"; Reactions API. Messages API "Message count and chronological ordering are tracked correctly across turns" is owned by `cpt-cf-mini-chat-feature-streaming-turns`; this feature provides the `message_count` query and the chronological default order it is tested against. Cleanup & Recovery "Chat deletion triggers reliable background cleanup" is owned by `cpt-cf-mini-chat-feature-background-workers`; this feature provides the transactional enqueue.
- **ADRs**: `cpt-cf-mini-chat-adr-canonical-error-contract`, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`, `cpt-cf-mini-chat-adr-document-retrieval-scope`, `cpt-cf-mini-chat-adr-quota-policy-scope`, `cpt-cf-mini-chat-adr-runtime-consistency-limitations`
- **Foundation elements reused by reference** (from `cpt-cf-mini-chat-feature-gear-foundation`, not redefined here): request gate `cpt-cf-mini-chat-flow-gear-foundation-request-gate`; PEP `cpt-cf-mini-chat-algo-gear-foundation-authorize`; owner-scoped chat and child access `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; DomainError to `Problem` mapping and its mapping table `cpt-cf-mini-chat-algo-gear-foundation-map-error`; plugin resolution `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`; outbox queues `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`; schema `cpt-cf-mini-chat-dod-gear-foundation-schema`
- **Dependencies**: `cpt-cf-mini-chat-feature-gear-foundation`

**Source precedence**: DESIGN is authoritative for details; where DESIGN delegates request and response schemas to the generated OpenAPI (`docs/api/api.json`), the schemas `CreateChatReq`, `UpdateChatReq`, `ChatDetailDto`, `Page_ChatDetailDto`, `MiniChatMessageDto`, `Page_MiniChatMessageDto`, `AttachmentSummaryDto`, `ImgThumbnailDto`, `SetReactionReq`, `MiniChatReactionDto` and `PageInfo` apply; the gear docs win over `config/mini-chat.yaml` and platform docs (DECOMPOSITION task-level overrides).

**REST contract** (all paths under `{url_prefix}`, default `/mini-chat`; every route requires authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`; PEP resource type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, `require_constraints` `true`):

| Method and path | PEP action, `resource.id` | Request | Success | Domain errors (besides gate, extractor and PEP errors) |
|---|---|---|---|---|
| `POST /v1/chats` | `create`, absent (properties `owner_tenant_id`, `owner_id`) | JSON `{ "title": string or null (optional), "model": string or null (optional) }` | 201, `ChatDetail`, header `Location: {url_prefix}/v1/chats/{id}` | 400 `INVALID_TITLE`, 400 `INVALID_MODEL`, 500 `internal` (policy plugin) |
| `GET /v1/chats` | `list`, absent | query `limit`, `cursor`, `$filter`, `$orderby` (`$select` accepted and ignored) | 200, `{ "items": [ChatDetail], "page_info": { "limit", "next_cursor", "prev_cursor" } }` | 400 OData `invalid_argument` |
| `GET /v1/chats/{id}` | `read`, chat id | none | 200, `ChatDetail` | 404 chat |
| `PATCH /v1/chats/{id}` | `update`, chat id | JSON `{ "title": string (required) }`, unknown fields ignored | 200, `ChatDetail` | 400 `INVALID_TITLE`, 404 chat |
| `DELETE /v1/chats/{id}` | `delete`, chat id | none | 204, no body | 404 chat, 400 cleanup payload too large |
| `GET /v1/chats/{id}/messages` | `list_messages`, chat id | query `limit`, `cursor`, `$filter`, `$orderby` (`$select` accepted and ignored) | 200, `{ "items": [Message], "page_info": { "limit", "next_cursor", "prev_cursor" } }` | 404 chat, 400 OData `invalid_argument` |
| `PUT /v1/chats/{id}/messages/{msg_id}/reaction` | `set_reaction`, chat id | JSON `{ "reaction": string (required) }` | 200, `{ "message_id", "reaction", "created_at" }` | 400 `INVALID_REACTION`, 404 chat, 404 message, 400 `failed_precondition` `reaction_target` |
| `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` | `delete_reaction`, chat id | none | 204, no body | 404 chat, 404 message, 400 `failed_precondition` `reaction_target` |

`ChatDetail` (`ChatDetailDto`) has exactly `id` (UUID string), `model` (string), `title` (string, omitted when the chat has no title, never `null`), `is_temporary` (boolean, always `false` in P1), `message_count` (integer), `created_at`, `updated_at` (RFC 3339 date-time); never `user_id`, `tenant_id` or `deleted_at`. `Message` (`MiniChatMessageDto`) has the required keys `id`, `request_id` (UUID, never `null`), `role` (`user` | `assistant` | `system`), `content`, `attachments` (array, `[]` when none), `my_reaction` (`like` | `dislike` | `null`, key always present), `created_at`, and the optional keys `model`, `input_tokens`, `output_tokens` (omitted when absent). `AttachmentSummary` has `attachment_id`, `kind` (`document` | `image`), `filename`, `status` (`pending` | `uploaded` | `ready` | `failed`) and `img_thumbnail` (`{ content_type, width, height, data_base64 }`, present only for `kind` `image` with `status` `ready`).

Pagination of both list endpoints: `limit` defaults to 20 and is at most 100 (a larger value is clamped to 100, not rejected; `0` returns 400 `INVALID_LIMIT`); chats filter and order over `updated_at`, `id`, `title` with default order `updated_at desc` then `id`; messages filter and order over `created_at`, `id`, `role` with default order `created_at asc` then `id asc`. Every OData failure is 400 `invalid_argument` with `context.resource_type` `gts.cf.core.odata.query.v1~` and the `field_violations[].reason` of the foundation mapping table (`INVALID_FILTER`, `INVALID_ORDERBY_FIELD`, `INVALID_LIMIT`, `INVALID_CURSOR`, `ORDER_MISMATCH`, `FILTER_MISMATCH`, `ORDER_WITH_CURSOR`, `FILTER_TOO_LONG`, `FILTER_TOO_COMPLEX`, `INVALID_SELECT`, `UNSUPPORTED_QUERY_PARAM`, `INVALID_QUERY_PARAMS`).

**Feature-local deviations and interpretations** (review owner: mini-chat gear maintainers):

| Topic | Deviation or interpretation | Rationale |
|-------|-----------------------------|-----------|
| `message_count` definition | Number of the chat's `messages` rows with `deleted_at IS NULL`, all roles (`user`, `assistant`, `system`) counted; messages soft-deleted by a turn retry, edit or delete are not counted | DESIGN names the field but not its formula; counting the same rows `GET /v1/chats/{id}/messages` returns keeps the two endpoints consistent |
| Title storage and length unit | The trimmed title is stored and returned (never the raw value); length is counted in Unicode characters (scalar values), not bytes; trimming removes leading and trailing Unicode whitespace | DESIGN validates "1–255 characters after trim" and the column is VARCHAR(255) in characters; storing the untrimmed value would let a stored title violate the rule that was checked |
| Title check order on `PATCH` | `PATCH /v1/chats/{id}` validates the title before authorization and the chat lookup, as `POST /v1/chats` does (an invalid title on an unknown chat returns 400, not 404) | DESIGN states this order explicitly only for create; one order for both title-bearing endpoints keeps validation free of PDP and database calls |
| `model` value forms on create | `"model": null` is treated like an absent `model` (default-model algorithm); any present string, including `""`, must equal a catalog `id` exactly (case-sensitive) of an `enabled` entry, otherwise 400 `INVALID_MODEL` | `CreateChatReq.model` is a nullable string in the OpenAPI; DESIGN requires a provided model to reference an enabled catalog id |
| Unknown create fields | Unknown fields in the `POST /v1/chats` body (including `is_temporary`) are ignored; the chat is always created with `is_temporary` `false` | DESIGN states the ignore rule for `PATCH`; temporary chats are P2 and the column default is `false` (B.10) |
| Attachments marked on chat delete | Only attachment rows of the chat whose `cleanup_status` is NULL are set to `pending`; rows already `pending` stay `pending` and from then on belong to the chat-cleanup path; terminal `done` / `failed` rows are not touched | DESIGN says "mark all attachments" but also defines `done` and `failed` as terminal with only `pending` to `done` / `failed` transitions, and transfers ownership of pending attachment-delete work to chat cleanup when the chat is soft-deleted |
| Null `request_id` on a stored message | `GET /v1/chats/{id}/messages` fails with 500 `internal` (the message id is logged) instead of filtering the row out | DESIGN allows "filtered out or backfilled" but its implementation guard requires failing the request; a silently missing message would corrupt history and `message_count` consistency |
| Order tiebreaker | When `$orderby` does not name `id`, `id` is appended in the direction of the first order key; the chat default order is therefore `updated_at desc, id desc` and the message default order `created_at asc, id asc` | DESIGN requires `id` as tiebreaker for chats and the strict total order `(created_at ASC, id ASC)` for messages; keyset cursors need a total order |
| `AttachmentSummary` details | Within one message the summaries are ordered by `message_attachments.created_at` ascending, then `attachment_id`; `img_thumbnail` is omitted (not `null`) when it does not apply; its `content_type` is `image/webp` | DESIGN fixes the contents but not the order; "present only for images with `status=ready`" and the WebP-only thumbnail storage |
| Reaction replace | A `PUT` on an existing `(message_id, user_id)` reaction keeps the row `id` and sets `reaction` and `created_at` to the new value and the time of the `PUT`; the response returns them | DESIGN specifies upsert ("it is replaced") without saying which timestamp the replaced reaction carries |
| `updated_at` on delete | Delete sets `chats.updated_at` to the deletion time (equal to `deleted_at`) | DESIGN lists delete among the bumps; the PRD sentence lists only create and rename; DESIGN is authoritative |
| PDP failure status | 503 `service_unavailable` with `Retry-After: 5` on every operation of this feature, not 403 as one sentence of `cpt-cf-mini-chat-nfr-tenant-isolation` states | Inherited unchanged from `cpt-cf-mini-chat-algo-gear-foundation-authorize` (see the foundation deviation table) |
| Chat cleanup before the handler exists | Chat-cleanup messages enqueued before `cpt-cf-mini-chat-feature-background-workers` lands are held by the foundation placeholder handler (`Retry`) and processed once the real handler replaces it; none is lost | The foundation registers the queue with a placeholder that never acknowledges; this feature only produces |

**External-call timeouts** (foundation gear constants, reused unchanged; every call to a dependency that can stall is bounded):

| Constant | Value | Call in this feature | Outcome on timeout |
|----------|-------|----------------------|--------------------|
| `PDP_EVALUATION_TIMEOUT` | 5 s | Every PEP evaluation (`create`, `list`, `read`, `update`, `delete`, `list_messages`, `set_reaction`, `delete_reaction`) through `cpt-cf-mini-chat-algo-gear-foundation-authorize` | 503 `service_unavailable`, `Retry-After: 5`, `context.retry_after_seconds = 5` |
| `REGISTRY_QUERY_TIMEOUT` | 10 s | Model-policy plugin resolution in `cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model` | 500 `internal`; nothing is cached; the chat is not created |
| `POLICY_PLUGIN_CALL_TIMEOUT` | 10 s | `get_current_policy_version` and `get_policy_snapshot` in `cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model` | 500 `internal`; the chat is not created |

Database statements and the outbox enqueue run inside the request transaction with the platform connection settings; a database or outbox failure rolls the transaction back and returns 500 `internal` (driver text only logged). No other external system is called by this feature.

**Audit and metrics**: this feature emits no audit event (chat deletion is not audited, `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`) and records no metric instrument (DESIGN "Metric series" defines none for chat, message-list or reaction operations). PDP failures are logged at `error` by the foundation PEP; internal errors are logged with the operation name and chat id, never with message content.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-mini-chat-usecase-delete-chat` (steps 1–3: soft delete, cleanup enqueue, 204; step 4 belongs to `cpt-cf-mini-chat-feature-background-workers`; step 5 is not implemented).

### Create Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-create-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- A body with a valid `title` and an enabled catalog `model` creates the chat on that model and returns 201 with the `ChatDetail` and a `Location` header.
- A body without `model` (absent or `null`) creates the chat on the default model (first enabled entry with `preference.is_default` `true`, else the first enabled entry).
- A body without `title` (absent or `null`) creates an untitled chat whose `ChatDetail` has no `title` key.

**Error Scenarios**:
- Empty, whitespace-only or longer-than-255-character `title`: 400 `invalid_argument`, `field_violations[title].reason = INVALID_TITLE`, before any PDP or catalog call.
- Unknown or disabled `model`, or no enabled model at all when `model` is omitted: 400 `invalid_argument`, `field_violations[model].reason = INVALID_MODEL`.
- Model-policy plugin missing, failing or timing out: 500 `internal`.
- Body that does not match the schema (for example `title` is a number): 422 `invalid_json_body`; malformed JSON: 400 `json_syntax_error`; non-JSON `Content-Type`: 415 `missing_json_content_type`.
- Gate and PEP errors of `cpt-cf-mini-chat-flow-gear-foundation-request-gate`: 401, 403 `LICENSE_FEATURE_REQUIRED`, 403 `AUTHZ_DENIED`, 503 `Retry-After: 5`.

**Steps**:
1. [x] - `p1` - User calls `POST {url_prefix}/v1/chats` with a JSON body `{ title, model }` (both optional and nullable) through the request gate (see `cpt-cf-mini-chat-flow-gear-foundation-request-gate`); unknown body fields, including `is_temporary`, are ignored - `inst-call-create`
2. [x] - `p1` - **IF** `title` is present and not `null` - `inst-if-title-present`
   1. [x] - `p1` - Validate it (see `cpt-cf-mini-chat-algo-chat-crud-validate-title`); **IF** invalid, **RETURN** 400 `INVALID_TITLE` without calling the PDP or the model-policy plugin - `inst-validate-create-title`
3. [x] - `p1` - Authorize the chat resource type, action `create`, no `resource.id`, `require_constraints` `true`, with `resource.properties.owner_tenant_id` and `owner_id` from the `SecurityContext` (see `cpt-cf-mini-chat-algo-gear-foundation-authorize`) - `inst-authorize-create`
4. [x] - `p1` - Resolve the chat model from the request `model` (see `cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model`); **IF** it returns an error, **RETURN** it (400 `INVALID_MODEL` or 500 `internal`) - `inst-resolve-model`
5. [x] - `p1` - DB: INSERT `chats` through Secure ORM with the compiled scope: new UUID `id`, `tenant_id` and `user_id` from the `SecurityContext`, `model` the resolved model id, `title` the trimmed title or NULL, `is_temporary` `false`, `created_at` and `updated_at` the same current time, `deleted_at` NULL - `inst-insert-chat`
6. [x] - `p1` - **IF** the scope refuses the insert (the row is outside the compiled scope) - `inst-if-insert-out-of-scope`
   1. [x] - `p1` - **RETURN** the denied domain error (403 `AUTHZ_DENIED`) - `inst-return-insert-denied`
7. [x] - `p1` - Project the new row with `message_count` 0 (see `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`) - `inst-project-created`
8. [x] - `p1` - **RETURN** 201 with the `ChatDetail` and the header `Location` equal to the request path plus `/{id}` (default `/mini-chat/v1/chats/{id}`, without the api-gateway `prefix_path`) - `inst-return-201`

### Get Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-get-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with the `ChatDetail` of an own, non-deleted chat, including `message_count`; no messages are embedded.

**Error Scenarios**:
- Unknown, soft-deleted, other-user or other-tenant chat: 404 `not_found`, `context.resource_type = gts.cf.core.mini_chat.chat.v1~`.
- Non-UUID `id`: 400 `invalid_path_params`; gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/chats/{id}` through the request gate; `id` must be a UUID - `inst-call-get-chat`
2. [x] - `p1` - Authorize the chat resource type, action `read`, `resource.id` the path `id`, `require_constraints` `true` - `inst-authorize-read`
3. [x] - `p1` - Load the chat through `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access` (no child lookup); **IF** not found, **RETURN** 404 with the chat resource type - `inst-load-chat-read`
4. [x] - `p1` - Project the chat with its `message_count` (see `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`) - `inst-project-read`
5. [x] - `p1` - **RETURN** 200 with the `ChatDetail` - `inst-return-chat`

### List Chats

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-list-chats`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with the caller's non-deleted chats, most recently active first (`updated_at desc`, then `id desc`), at most `limit` items, and `page_info` with the effective `limit` and the `next_cursor` / `prev_cursor` to follow.
- `$filter` over `updated_at`, `id`, `title` (for example `contains(title, 'report')`) and `$orderby` over the same fields narrow and reorder the list; following `next_cursor` returns the next page without duplicates or gaps.
- An empty result is 200 with `items` `[]` and `next_cursor` `null`.

**Error Scenarios**:
- `limit=0`, an unknown field or malformed expression in `$filter` / `$orderby`, a malformed or mismatched cursor, `cursor` combined with `$orderby`, an unsupported `$` option (for example `$skip`, `$count`), a malformed `$select`: 400 `invalid_argument` with `context.resource_type = gts.cf.core.odata.query.v1~` and the OData reason.
- Gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/chats` with optional `limit`, `cursor`, `$filter`, `$orderby` (and `$select`, syntax-checked and ignored) through the request gate; the platform OData extractor rejects an unparsable query string, an unsupported `$` option, a too long or too complex `$filter` and a malformed `$select` - `inst-call-list-chats`
2. [x] - `p1` - Authorize the chat resource type, action `list`, no `resource.id`, `require_constraints` `true` - `inst-authorize-list-chats`
3. [x] - `p1` - Page the `chats` rows through Secure ORM with the compiled scope and `deleted_at IS NULL` (see `cpt-cf-mini-chat-algo-chat-crud-paginate`) with allowed fields `updated_at`, `id`, `title` and default order `updated_at desc` then `id desc` - `inst-page-chats`
4. [x] - `p1` - Project every chat of the page with its `message_count` in one batch (see `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`) - `inst-project-chat-page`
5. [x] - `p1` - **RETURN** 200 with `{ items: [ChatDetail, ...], page_info: { limit, next_cursor, prev_cursor } }` in page order - `inst-return-chat-page`

### Rename Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-rename-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with the updated `ChatDetail`: new trimmed `title`, `updated_at` set to now, `model`, `is_temporary`, `created_at` and `message_count` unchanged.
- A body such as `{"title": "Renamed", "model": "other"}` renames the chat and leaves `model` unchanged.

**Error Scenarios**:
- Empty, whitespace-only or longer-than-255-character `title`: 400 `INVALID_TITLE`.
- Missing `title`, `title` `null` or not a string: 422 `invalid_json_body`; malformed JSON: 400 `json_syntax_error`; non-JSON `Content-Type`: 415.
- Unknown, soft-deleted or foreign chat: 404 with the chat resource type; gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `PATCH {url_prefix}/v1/chats/{id}` with a JSON body `{ title }` (`title` a required non-null string) through the request gate; unknown body fields such as `model` or `is_temporary` are ignored - `inst-call-rename`
2. [x] - `p1` - Validate the title (see `cpt-cf-mini-chat-algo-chat-crud-validate-title`); **IF** invalid, **RETURN** 400 `INVALID_TITLE` without calling the PDP - `inst-validate-rename-title`
3. [x] - `p1` - Authorize the chat resource type, action `update`, `resource.id` the path `id`, `require_constraints` `true` - `inst-authorize-update`
4. [x] - `p1` - Load the chat through `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; **IF** not found, **RETURN** 404 with the chat resource type - `inst-load-chat-update`
5. [x] - `p1` - DB: UPDATE `chats` SET `title` to the trimmed title and `updated_at` to the current time through Secure ORM with the scope, WHERE `id` is the chat id and `deleted_at IS NULL`; no other column is written - `inst-update-title`
6. [x] - `p1` - **IF** the update affected 0 rows (the chat was deleted concurrently) - `inst-if-update-lost`
   1. [x] - `p1` - **RETURN** 404 with the chat resource type - `inst-return-update-404`
7. [x] - `p1` - Project the updated chat with its `message_count` (see `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`) - `inst-project-updated`
8. [x] - `p1` - **RETURN** 200 with the updated `ChatDetail` - `inst-return-renamed`

### Delete Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-delete-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 204 with no body; the chat disappears from `GET /v1/chats`, and `GET`, `PATCH`, `DELETE` on it and every sub-resource route (messages, reactions, attachments, turns) return 404 with the chat resource type; one chat-cleanup message is durably enqueued.
- A turn running in the chat is not cancelled; it is finalized and billed normally by its owning feature.

**Error Scenarios**:
- Unknown, already deleted (second `DELETE`) or foreign chat: 404 with the chat resource type.
- Chat-cleanup payload larger than the outbox payload size limit: 400 `invalid_argument` with the message in `detail` and `context.format`; nothing is deleted.
- Database or outbox failure: 500 `internal`; nothing is deleted. Gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `DELETE {url_prefix}/v1/chats/{id}` (no body) through the request gate - `inst-call-delete`
2. [x] - `p1` - Authorize the chat resource type, action `delete`, `resource.id` the path `id`, `require_constraints` `true` - `inst-authorize-delete`
3. [x] - `p1` - Soft-delete the chat and enqueue its cleanup in one transaction (see `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`) - `inst-soft-delete`
4. [x] - `p1` - **IF** the soft delete returns a domain error - `inst-if-delete-error`
   1. [x] - `p1` - **RETURN** the `Problem` built by `cpt-cf-mini-chat-algo-gear-foundation-map-error` (404 chat, 400 cleanup payload too large, 500 internal) - `inst-return-delete-error`
5. [x] - `p1` - **RETURN** 204 No Content with an empty body - `inst-return-204`

### List Messages

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-list-messages`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with the chat's non-deleted messages in chronological order (`created_at asc`, then `id asc`), each carrying `request_id`, `attachments` and `my_reaction`, and `page_info` to follow.
- `$filter` over `created_at`, `id`, `role` (for example `role eq 'assistant'`, `id eq '{uuid}'`) and `$orderby` over the same fields narrow and reorder the page; `limit` above 100 returns at most 100 items with `page_info.limit` 100.

**Error Scenarios**:
- Unknown, soft-deleted or foreign chat: 404 with the chat resource type.
- OData errors as in List Chats (400, `gts.cf.core.odata.query.v1~`).
- A stored message of the page with a NULL `request_id`: 500 `internal`. Gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `GET {url_prefix}/v1/chats/{id}/messages` with optional `limit`, `cursor`, `$filter`, `$orderby` (and `$select`, syntax-checked and ignored) through the request gate - `inst-call-list-messages`
2. [x] - `p1` - Authorize the chat resource type, action `list_messages`, `resource.id` the path chat `id`, `require_constraints` `true` - `inst-authorize-list-messages`
3. [x] - `p1` - Load the chat through `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`; **IF** not found, **RETURN** 404 with the chat resource type - `inst-load-chat-messages`
4. [x] - `p1` - Page the `messages` rows through Secure ORM (tenant column `tenant_id`) filtered by the loaded chat's `id` and `deleted_at IS NULL` (see `cpt-cf-mini-chat-algo-chat-crud-paginate`) with allowed fields `created_at`, `id`, `role` and default order `created_at asc` then `id asc` - `inst-page-messages`
5. [x] - `p1` - Enrich the page into `Message` objects (see `cpt-cf-mini-chat-algo-chat-crud-enrich-messages`); **IF** it returns an internal error, **RETURN** 500 `internal` - `inst-enrich-page`
6. [x] - `p1` - **RETURN** 200 with `{ items: [Message, ...], page_info: { limit, next_cursor, prev_cursor } }` in page order - `inst-return-message-page`

### Set Reaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-set-reaction`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 200 with `{ message_id, reaction, created_at }` for an assistant message of an own chat; a second `PUT` with the other value replaces the reaction (one row per user and message); a repeated `PUT` with the same value succeeds again.

**Error Scenarios**:
- `reaction` is a string other than `like` or `dislike` (case-sensitive): 400 `invalid_argument`, `field_violations[reaction].reason = INVALID_REACTION`, before any PDP call.
- Body without `reaction` or with a non-string `reaction`: 422 `invalid_json_body`; malformed JSON: 400; non-JSON `Content-Type`: 415.
- Unknown, soft-deleted or foreign chat: 404 with the chat resource type; message unknown, soft-deleted or in another chat: 404 with `gts.cf.core.mini_chat.message.v1~`.
- User or system message: 400 `failed_precondition`, `violations[{subject: reaction_target, type: STATE}]`. Gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `PUT {url_prefix}/v1/chats/{id}/messages/{msg_id}/reaction` with a JSON body `{ reaction }` (a required string) through the request gate; both path parameters must be UUIDs - `inst-call-set-reaction`
2. [x] - `p1` - **IF** `reaction` is not exactly `like` or `dislike` - `inst-if-bad-reaction`
   1. [x] - `p1` - **RETURN** 400 `INVALID_REACTION` without calling the PDP - `inst-return-invalid-reaction`
3. [x] - `p1` - Authorize the chat resource type, action `set_reaction`, `resource.id` the path chat `id`, `require_constraints` `true` - `inst-authorize-set-reaction`
4. [x] - `p1` - Resolve the target message (see `cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target`); **IF** it returns an error, **RETURN** it (404 chat, 404 message, 400 `reaction_target`) - `inst-resolve-set-target`
5. [x] - `p1` - DB: upsert `message_reactions` through Secure ORM (tenant column `tenant_id`, owner column `user_id`) in one statement: insert a row with a new UUID `id`, `message_id` the target message id, `user_id` the subject id, `tenant_id` the chat's `tenant_id`, `reaction` and `created_at` the current time; on conflict with UNIQUE `(message_id, user_id)` update only `reaction` and `created_at` - `inst-upsert-reaction`
6. [x] - `p1` - Leave `chats.updated_at` and the message row unchanged - `inst-no-chat-bump`
7. [x] - `p1` - **RETURN** 200 with `{ message_id, reaction, created_at }` of the stored row - `inst-return-reaction`

### Remove Reaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-crud-remove-reaction`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- 204 with no body for an assistant message of an own chat, whether or not the caller had a reaction on it (idempotent); another user's reaction on the same message is never removed.

**Error Scenarios**:
- The same 404 and `reaction_target` errors as Set Reaction (a `DELETE` on a user or system message is 400 `failed_precondition`). Gate and PEP errors as in Create Chat.

**Steps**:
1. [x] - `p1` - User calls `DELETE {url_prefix}/v1/chats/{id}/messages/{msg_id}/reaction` (no body) through the request gate; both path parameters must be UUIDs - `inst-call-remove-reaction`
2. [x] - `p1` - Authorize the chat resource type, action `delete_reaction`, `resource.id` the path chat `id`, `require_constraints` `true` - `inst-authorize-delete-reaction`
3. [x] - `p1` - Resolve the target message (see `cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target`); **IF** it returns an error, **RETURN** it - `inst-resolve-remove-target`
4. [x] - `p1` - DB: DELETE FROM `message_reactions` through Secure ORM WHERE `tenant_id` is the chat's tenant, `user_id` is the subject id and `message_id` is the target message id; 0 affected rows is success - `inst-delete-reaction`
5. [x] - `p1` - **RETURN** 204 No Content with an empty body - `inst-return-reaction-204`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Title Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-validate-title`

**Input**: Raw `title` string from `POST /v1/chats` or `PATCH /v1/chats/{id}`

**Output**: Trimmed title, or the invalid-title domain error (400 `invalid_argument`, `field_violations[title].reason = INVALID_TITLE`)

**Steps**:
1. [x] - `p1` - Remove leading and trailing Unicode whitespace - `inst-trim-title`
2. [x] - `p1` - **IF** the trimmed title has 0 characters (empty or whitespace-only input) - `inst-if-title-empty`
   1. [x] - `p1` - **RETURN** the invalid-title error - `inst-return-title-empty`
3. [x] - `p1` - **IF** the trimmed title has more than 255 characters (Unicode scalar values, not bytes) - `inst-if-title-long`
   1. [x] - `p1` - **RETURN** the invalid-title error - `inst-return-title-long`
4. [x] - `p1` - **RETURN** the trimmed title (the value that is stored and returned) - `inst-return-title`

### Chat Model Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model`

**Input**: Optional `model` string from the create request (`null` treated as absent), `SecurityContext` (user id for plugin calls)

**Output**: The model id to store as `chats.model` (the selected model), the invalid-model domain error (400 `invalid_argument`, `field_violations[model].reason = INVALID_MODEL`), or an internal error (500)

**Steps**:
1. [x] - `p1` - Resolve the model-policy plugin (see `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`); **IF** it is not registered or resolution fails (including `REGISTRY_QUERY_TIMEOUT`), **RETURN** internal error - `inst-resolve-policy-plugin`
2. [x] - `p1` - Read the current policy version and then the snapshot of that version (no local cache), each call bounded by `POLICY_PLUGIN_CALL_TIMEOUT` (10 s); **IF** either call fails or the timeout elapses, **RETURN** internal error - `inst-read-catalog`
3. [x] - `p1` - **IF** a `model` value is present - `inst-if-model-given`
   1. [x] - `p1` - **IF** no `model_catalog` entry has `id` exactly equal to the value, or that entry has `enabled` `false` - `inst-if-model-invalid`
      1. [x] - `p1` - **RETURN** the invalid-model error - `inst-return-model-invalid`
   2. [x] - `p1` - **RETURN** the given value - `inst-return-given-model`
4. [x] - `p1` - **FOR EACH** `model_catalog` entry in catalog order - `inst-for-each-default-candidate`
   1. [x] - `p1` - **IF** the entry has `enabled` `true` and `preference.is_default` `true` (an absent `preference` counts as not default) - `inst-if-default-entry`
      1. [x] - `p1` - **RETURN** the entry `id` - `inst-return-default-entry`
5. [x] - `p1` - **IF** any entry has `enabled` `true` - `inst-if-any-enabled`
   1. [x] - `p1` - **RETURN** the `id` of the first enabled entry in catalog order - `inst-return-first-enabled`
6. [x] - `p1` - **RETURN** the invalid-model error (no enabled model exists); `tier` is never considered by this algorithm - `inst-return-no-enabled`

### Chat Detail Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`

**Input**: One or more `chats` rows (one row for create, get and rename; a page for list), the request tenant id

**Output**: `ChatDetail` objects in input order

**Steps**:
1. [x] - `p1` - **IF** the input is a single newly created chat - `inst-if-new-chat`
   1. [x] - `p1` - Use `message_count` 0 without a query - `inst-new-chat-count`
2. [x] - `p1` - **ELSE** - `inst-else-count-query`
   1. [x] - `p1` - DB: SELECT `chat_id` and COUNT of `messages` through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is in the input chat ids and `deleted_at IS NULL`, grouped by `chat_id` (one query per request, never one per chat) - `inst-count-messages`
   2. [x] - `p1` - Use 0 for a chat without rows - `inst-default-count`
3. [x] - `p1` - **FOR EACH** chat - `inst-for-each-chat`
   1. [x] - `p1` - Project exactly `id`, `model`, `title`, `is_temporary`, `message_count`, `created_at`, `updated_at` (RFC 3339 UTC); omit the `title` key when `title` is NULL (never send `null`) - `inst-project-chat-fields`
4. [x] - `p1` - Never project `tenant_id`, `user_id` or `deleted_at` - `inst-chat-non-exposure`
5. [x] - `p1` - **RETURN** the projections - `inst-return-chat-details`

### Cursor Pagination and OData Query

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-paginate`

**Input**: Scoped base query (chats: owner scope and `deleted_at IS NULL`; messages: tenant scope, `chat_id` and `deleted_at IS NULL`), allowed field set, default order, request `limit`, `cursor`, `$filter`, `$orderby`

**Output**: Rows of one page plus `page_info` (`limit`, `next_cursor`, `prev_cursor`), or an OData query domain error (400 `invalid_argument`, `context.resource_type = gts.cf.core.odata.query.v1~`)

**Steps**:
1. [x] - `p1` - **IF** `limit` is absent - `inst-if-limit-absent`
   1. [x] - `p1` - Use 20 - `inst-default-limit`
2. [x] - `p1` - **IF** `limit` is 0 - `inst-if-limit-zero`
   1. [x] - `p1` - **RETURN** OData error `INVALID_LIMIT` (field `$top`) - `inst-return-invalid-limit`
3. [x] - `p1` - **IF** `limit` is greater than 100 - `inst-if-limit-large`
   1. [x] - `p1` - Clamp it to 100 (not an error) - `inst-clamp-limit`
4. [x] - `p1` - **IF** both `cursor` and `$orderby` are present - `inst-if-order-with-cursor`
   1. [x] - `p1` - **RETURN** OData error `ORDER_WITH_CURSOR` - `inst-return-order-with-cursor`
5. [x] - `p1` - Parse `$filter` with the platform OData library; **IF** it is malformed or names a field outside the allowed set, **RETURN** OData error `INVALID_FILTER` - `inst-compile-filter`
6. [x] - `p1` - Parse `$orderby`; **IF** it is malformed or names a field outside the allowed set, **RETURN** OData error `INVALID_ORDERBY_FIELD`; otherwise use it as the order, else the default order, and append `id` in the direction of the first order key when the order does not name `id` - `inst-compile-order`
7. [x] - `p1` - **IF** `cursor` is present - `inst-if-cursor`
   1. [x] - `p1` - Decode the opaque cursor; **IF** it is malformed, **RETURN** OData error `INVALID_CURSOR` - `inst-decode-cursor`
   2. [x] - `p1` - **IF** the order recorded in the cursor differs from the effective order, **RETURN** `ORDER_MISMATCH`; **IF** the filter recorded in the cursor differs from the request `$filter`, **RETURN** `FILTER_MISMATCH` - `inst-check-cursor-match`
   3. [x] - `p1` - Continue in the cursor's direction (next or previous page) from the key values the cursor carries - `inst-apply-cursor`
8. [x] - `p1` - DB: SELECT from the scoped base query with the compiled filter, the cursor key condition (keyset over the order keys), the effective order and LIMIT `limit` plus 1 - `inst-select-page`
9. [x] - `p1` - **IF** more than `limit` rows are returned - `inst-if-more-rows`
   1. [x] - `p1` - Drop the extra row and set `next_cursor` to an opaque cursor encoding the last returned row's order keys, the effective order and the filter - `inst-set-next-cursor`
10. [x] - `p1` - **ELSE** - `inst-else-last-page`
    1. [x] - `p1` - Set `next_cursor` to `null` - `inst-null-next-cursor`
11. [x] - `p1` - Set `prev_cursor` to `null` on the first page (no `cursor` in the request) and otherwise to the cursor of the page before the first returned row, as the platform OData library builds it - `inst-set-prev-cursor`
12. [x] - `p1` - Report every OData library or extractor error raised here or while paginating in the repository as the OData query domain error with its `field_violations[].reason` (see the "Bad OData query" row of `cpt-cf-mini-chat-algo-gear-foundation-map-error`) - `inst-map-odata-errors`
13. [x] - `p1` - **RETURN** the rows and `page_info` with the effective `limit` - `inst-return-page`

### Chat Soft Delete and Cleanup Enqueue

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`

**Input**: `AccessScope` from the `delete` authorization, chat id, request tenant id

**Output**: Success, or a domain error: not found (chat), cleanup payload too large, internal

**Steps**:
1. [x] - `p1` - Begin one database transaction for all following writes - `inst-begin-delete-tx`
2. [x] - `p1` - Load the chat through `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access` inside the transaction; **IF** not found (unknown, foreign or already soft-deleted), roll back and **RETURN** not found with the chat resource type - `inst-load-chat-delete`
3. [x] - `p1` - Take the current time once as the deletion time - `inst-take-delete-time`
4. [x] - `p1` - DB: UPDATE `chats` SET `deleted_at` and `updated_at` to the deletion time through Secure ORM with the scope, WHERE `id` is the chat id and `deleted_at IS NULL` - `inst-mark-chat-deleted`
5. [x] - `p1` - **IF** the update affected 0 rows (a concurrent delete won) - `inst-if-delete-lost`
   1. [x] - `p1` - Roll back and **RETURN** not found with the chat resource type - `inst-return-delete-lost`
6. [x] - `p1` - DB: UPDATE `attachments` SET `cleanup_status` to `pending` and `cleanup_updated_at` to the deletion time through Secure ORM (tenant column `tenant_id`) WHERE `chat_id` is the chat id and `cleanup_status IS NULL`, for every kind and status - `inst-mark-attachments-pending`
7. [x] - `p1` - Build the chat-cleanup payload with exactly `tenant_id` (the chat's tenant), `chat_id`, `system_request_id` (a new server-generated UUID v4, fixed for every later retry of this message), `reason` `chat_soft_delete` and `chat_deleted_at` (the deletion time) - `inst-build-cleanup-payload`
8. [x] - `p1` - **TRY** - `inst-try-enqueue`
   1. [x] - `p1` - Enqueue the payload through the shared outbox enqueuer in the same transaction to queue `outbox.chat_cleanup_queue_name` (default `mini-chat.chat_cleanup`) with partition key `chat_id` - `inst-enqueue-cleanup`
9. [x] - `p1` - **CATCH** the outbox rejects the payload as larger than its payload size limit - `inst-catch-payload-too-large`
   1. [x] - `p1` - Roll back and **RETURN** the cleanup-payload-too-large error (400 `invalid_argument`, the message in `detail` and in `context.format`) - `inst-return-payload-too-large`
10. [x] - `p1` - **CATCH** any database or outbox error - `inst-catch-delete-failure`
    1. [x] - `p1` - Roll back, log the cause with the chat id and **RETURN** internal error - `inst-return-delete-internal`
11. [x] - `p1` - Commit the transaction - `inst-commit-delete`
12. [x] - `p1` - Never write `messages`, `chat_turns`, `message_attachments`, `message_reactions`, `thread_summaries`, `chat_vector_stores` or `attachments.deleted_at`, never cancel a running turn, never call a provider and never enqueue an audit event during the delete - `inst-delete-boundaries`
13. [x] - `p1` - **RETURN** success - `inst-return-deleted`

### Message Page Enrichment

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-enrich-messages`

**Input**: One page of `messages` rows (in page order), the loaded chat, `SecurityContext`

**Output**: `Message` objects in page order, or an internal error

**Steps**:
1. [x] - `p1` - **IF** the page is empty - `inst-if-page-empty`
   1. [x] - `p1` - **RETURN** an empty list without further queries - `inst-return-empty-page`
2. [x] - `p1` - **FOR EACH** message of the page - `inst-for-each-request-id`
   1. [x] - `p1` - **IF** `request_id` is NULL, log an error naming the message id and **RETURN** internal error (a message is never serialized with a null `request_id`) - `inst-guard-request-id`
3. [x] - `p1` - DB: SELECT `message_attachments` INNER JOIN `attachments` ON `attachment_id` and `chat_id` through Secure ORM (tenant column `tenant_id`) WHERE `message_attachments.chat_id` is the chat id, `message_id` is in the page message ids and `attachments.deleted_at IS NULL`, ordered by `message_attachments.created_at` then `attachment_id`; one query per page - `inst-select-attachments`
4. [x] - `p1` - Build each `AttachmentSummary` with `attachment_id`, `kind` (from `attachment_kind`), `filename`, `status`, and `img_thumbnail` `{ content_type: image/webp, width, height, data_base64 }` only when `kind` is `image`, `status` is `ready` and thumbnail bytes are stored; otherwise omit `img_thumbnail` - `inst-build-attachment-summary`
5. [x] - `p1` - DB: SELECT `message_id` and `reaction` from `message_reactions` through Secure ORM (tenant column `tenant_id`, owner column `user_id`) WHERE `user_id` is the subject id and `message_id` is in the assistant message ids of the page; one query per page - `inst-select-reactions`
6. [x] - `p1` - **FOR EACH** message of the page - `inst-for-each-message`
   1. [x] - `p1` - Project the required keys `id`, `request_id`, `role`, `content`, `created_at` (RFC 3339 UTC) - `inst-project-message-core`
   2. [x] - `p1` - Set `attachments` to the message's summaries, or to an empty array when it has none (always the case for assistant and system messages) - `inst-set-attachments`
   3. [x] - `p1` - **IF** `role` is `assistant` and the caller has a reaction on the message - `inst-if-has-reaction`
      1. [x] - `p1` - Set `my_reaction` to `like` or `dislike` - `inst-set-my-reaction`
   4. [x] - `p1` - **ELSE** - `inst-else-no-reaction`
      1. [x] - `p1` - Set `my_reaction` to `null` (the key is always present; user and system messages are always `null`) - `inst-null-my-reaction`
   5. [x] - `p1` - Add `model` from `messages.model` only when it is not NULL (absent for user messages), and `input_tokens` / `output_tokens` only when the stored value is not 0 - `inst-optional-message-fields`
7. [x] - `p1` - Never project `tenant_id`, `chat_id`, `provider_response_id`, `content_type`, `token_estimate`, `request_kind`, `features_used`, cache or reasoning token counts, `is_compressed`, `deleted_at`, other users' reactions or any provider identifier - `inst-message-non-exposure`
8. [x] - `p1` - **RETURN** the `Message` objects in page order - `inst-return-messages`

### Reaction Target Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target`

**Input**: `AccessScope` from the `set_reaction` or `delete_reaction` authorization, chat id, `msg_id`

**Output**: The target assistant message row, or a domain error: not found (chat or message), reaction-target precondition

**Steps**:
1. [x] - `p1` - Load the chat and the child message through `cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access` (child table `messages`, filtered by the chat id, `msg_id` and `deleted_at IS NULL`) - `inst-load-target`
2. [x] - `p1` - **IF** the chat is not found - `inst-if-target-chat-missing`
   1. [x] - `p1` - **RETURN** not found with resource type `gts.cf.core.mini_chat.chat.v1~` - `inst-return-target-chat-404`
3. [x] - `p1` - **IF** the message is not found (unknown, soft-deleted or in another chat) - `inst-if-target-message-missing`
   1. [x] - `p1` - **RETURN** not found with resource type `gts.cf.core.mini_chat.message.v1~` - `inst-return-target-message-404`
4. [x] - `p1` - **IF** the message `role` is not `assistant` - `inst-if-not-assistant`
   1. [x] - `p1` - **RETURN** the reaction-target precondition error: 400 `failed_precondition` with `context.violations[{subject: reaction_target, type: STATE, description}]` - `inst-return-reaction-target`
5. [x] - `p1` - **RETURN** the message row - `inst-return-target`

## 4. States (CDSL)

### Chat Lifecycle State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-chat-crud-chat-lifecycle`

**States**: `active`, `soft_deleted`

**Initial State**: `active` (entered by a successful `POST /v1/chats`; `model` and `is_temporary` are fixed on entry)

**Transitions**:
1. [x] - `p1` - **FROM** `active` **TO** `active` **WHEN** `PATCH /v1/chats/{id}` succeeds: `title` and `updated_at` change, `model`, `is_temporary` and `created_at` do not - `inst-rename-active`
2. [x] - `p1` - **FROM** `active` **TO** `active` **WHEN** a sent message, retry or edit commits in the chat (owned by later features): `updated_at` is bumped, nothing else of the chat row changes - `inst-activity-active`
3. [x] - `p1` - **FROM** `active` **TO** `soft_deleted` **WHEN** `DELETE /v1/chats/{id}` commits: `deleted_at` and `updated_at` are set, attachments are marked for cleanup and the chat-cleanup message is enqueued in the same transaction - `inst-to-soft-deleted`
4. [x] - `p1` - **FROM** `soft_deleted` **TO** `soft_deleted` **WHEN** any chat or sub-resource operation targets the chat: it returns 404 with the chat resource type and changes nothing (no restore, no hard purge) - `inst-deleted-terminal`

### Message Reaction State Machine

- [x] `p1` - **ID**: `cpt-cf-mini-chat-state-chat-crud-message-reaction`

**States**: `none`, `like`, `dislike` (per assistant message and user; `none` means no `message_reactions` row)

**Initial State**: `none`

**Transitions**:
1. [x] - `p1` - **FROM** `none` **TO** `like` **WHEN** `PUT` with `reaction` `like` succeeds (row inserted) - `inst-none-to-like`
2. [x] - `p1` - **FROM** `none` **TO** `dislike` **WHEN** `PUT` with `reaction` `dislike` succeeds (row inserted) - `inst-none-to-dislike`
3. [x] - `p1` - **FROM** `like` **TO** `dislike` **WHEN** `PUT` with `reaction` `dislike` succeeds (row updated in place) - `inst-like-to-dislike`
4. [x] - `p1` - **FROM** `dislike` **TO** `like` **WHEN** `PUT` with `reaction` `like` succeeds (row updated in place) - `inst-dislike-to-like`
5. [x] - `p1` - **FROM** `like` **TO** `like` **WHEN** `PUT` repeats `like` (idempotent, `created_at` refreshed); the same holds for `dislike` - `inst-repeat-same`
6. [x] - `p1` - **FROM** `like` **TO** `none` **WHEN** `DELETE` succeeds; the same holds from `dislike` - `inst-to-none`
7. [x] - `p1` - **FROM** `none` **TO** `none` **WHEN** `DELETE` is called without an existing reaction (204, idempotent) - `inst-none-stays-none`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Chat Creation and Model Lock

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-create-chat`

The system **MUST** serve `POST /v1/chats` under `url_prefix` as specified in `cpt-cf-mini-chat-flow-chat-crud-create-chat`: title validated (trimmed, 1–255 characters, `INVALID_TITLE`) before the PEP and the catalog read; PEP action `create` with `owner_tenant_id` / `owner_id` properties; the model taken from the request only when it names an enabled catalog entry, otherwise `INVALID_MODEL`; the default model chosen deterministically from the catalog order of the current policy snapshot (first enabled `preference.is_default` entry, else first enabled entry, else `INVALID_MODEL`; tier not considered); the insert scoped by the compiled `AccessScope`; 201 with the `ChatDetail` and the `Location` header. `chats.model` is written only by this insert: no repository method, endpoint or later feature updates it (`cpt-cf-mini-chat-constraint-model-locked-per-chat`; the effective model of a turn is recorded on `messages.model` by the streaming feature).

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-create-chat`
- `cpt-cf-mini-chat-algo-chat-crud-validate-title`
- `cpt-cf-mini-chat-algo-chat-crud-resolve-chat-model`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `POST /v1/chats` (`cpt-cf-mini-chat-interface-public-api`)
- DB Table: `cpt-cf-mini-chat-dbtable-chats`
- Entities: Chat
- Design: `cpt-cf-mini-chat-design-model-catalog`, `cpt-cf-mini-chat-design-authz-pep`
- Components: `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-chat-store`, `cpt-cf-mini-chat-component-authz-integration`

### Chat Read, List and Rename

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-read-list-rename`

The system **MUST** serve `GET /v1/chats/{id}`, `GET /v1/chats` and `PATCH /v1/chats/{id}` as specified in their flows: PEP actions `read`, `list` and `update`; 404 with the chat resource type for unknown, soft-deleted and foreign chats; the `ChatDetail` projection with a batched `message_count` and `title` omitted when absent; list ordered by `updated_at desc, id desc` by default, filtered and ordered only over `updated_at`, `id`, `title`, cursor-paginated with `limit` default 20 and maximum 100 (clamped), `limit=0` and every malformed OData input rejected with 400 and `gts.cf.core.odata.query.v1~`; rename writing only `title` and `updated_at`, ignoring unknown body fields, never touching messages or attachments.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-get-chat`
- `cpt-cf-mini-chat-flow-chat-crud-list-chats`
- `cpt-cf-mini-chat-flow-chat-crud-rename-chat`
- `cpt-cf-mini-chat-algo-chat-crud-project-chat-detail`
- `cpt-cf-mini-chat-algo-chat-crud-paginate`
- `cpt-cf-mini-chat-state-chat-crud-chat-lifecycle`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `GET /v1/chats`, `GET /v1/chats/{id}`, `PATCH /v1/chats/{id}`
- DB Table: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages` (count only)
- Entities: Chat
- Components: `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-chat-store`

### Soft Delete with Cleanup Enqueue

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-delete-chat`

The system **MUST** serve `DELETE /v1/chats/{id}` as specified in `cpt-cf-mini-chat-flow-chat-crud-delete-chat` and `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`, implementing the synchronous half of `cpt-cf-mini-chat-seq-chat-deletion-cleanup`: in one transaction the chat row is soft-deleted (`deleted_at` and `updated_at` set), the chat's attachments without a cleanup status are marked `cleanup_status = pending`, and one chat-cleanup message (`tenant_id`, `chat_id`, `system_request_id`, `reason = chat_soft_delete`, `chat_deleted_at`) is enqueued to `outbox.chat_cleanup_queue_name` partitioned by `chat_id`; the response is 204; a second delete is 404; an oversized payload is 400 with `context.format` and leaves the chat intact; child rows are not modified, a running turn is not cancelled, nothing is hard-purged and no audit event is emitted (`cpt-cf-mini-chat-adr-data-lifecycle-audit-scope`). The handler that consumes the message is owned by `cpt-cf-mini-chat-feature-background-workers`; the payload type is shared with it.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-delete-chat`
- `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`
- `cpt-cf-mini-chat-state-chat-crud-chat-lifecycle`

**Touches**:
- API: `DELETE /v1/chats/{id}`
- DB Table: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments` (`cleanup_status`, `cleanup_updated_at` only)
- DB: shared outbox tables (queue `mini-chat.chat_cleanup`)
- Sequence: `cpt-cf-mini-chat-seq-chat-deletion-cleanup` (soft delete and enqueue part)
- Entities: Chat, Attachment
- Components: `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-chat-store`

### Messages API

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-messages-api`

The system **MUST** serve `GET /v1/chats/{id}/messages` as specified in `cpt-cf-mini-chat-flow-chat-crud-list-messages` and `cpt-cf-mini-chat-algo-chat-crud-enrich-messages`: PEP action `list_messages` on the chat; non-deleted messages only; default order `created_at asc, id asc`; filter and order only over `created_at`, `id`, `role`; the shared cursor pagination with limit clamping; every `Message` with a non-null `request_id`, an always-present `attachments` array built from `message_attachments` joined to non-deleted `attachments` in one batch query, an always-present `my_reaction` built from the caller's `message_reactions` rows in one batch query (`null` for user and system messages), and `model`, `input_tokens`, `output_tokens` only when present; a stored NULL `request_id` fails the request with 500; no provider identifier is ever returned (`cpt-cf-mini-chat-nfr-tenant-isolation`).

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-list-messages`
- `cpt-cf-mini-chat-algo-chat-crud-enrich-messages`
- `cpt-cf-mini-chat-algo-chat-crud-paginate`

**Touches**:
- API: `GET /v1/chats/{id}/messages`
- DB Table: `cpt-cf-mini-chat-dbtable-messages`, `cpt-cf-mini-chat-dbtable-message-attachments`, `cpt-cf-mini-chat-dbtable-attachments` (read only), `cpt-cf-mini-chat-dbtable-message-reactions` (read only)
- Entities: Message, Attachment, MessageReaction

### Reactions API

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-reactions`

The system **MUST** serve `PUT` and `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` (`cpt-cf-mini-chat-interface-message-reaction`) as specified in their flows: `INVALID_REACTION` checked before the PEP; PEP actions `set_reaction` and `delete_reaction` on the chat; 404 for the chat or the message (`gts.cf.core.mini_chat.message.v1~`); 400 `failed_precondition` `reaction_target` / `STATE` for a user or system message on both methods; an atomic upsert on UNIQUE `(message_id, user_id)` so each user has at most one reaction per message; `tenant_id` and `user_id` taken from the resolved chat and the security context, never from input; `DELETE` idempotent with 204; `chats.updated_at` unchanged.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-set-reaction`
- `cpt-cf-mini-chat-flow-chat-crud-remove-reaction`
- `cpt-cf-mini-chat-algo-chat-crud-resolve-reaction-target`
- `cpt-cf-mini-chat-state-chat-crud-message-reaction`

**Touches**:
- API: `PUT /v1/chats/{id}/messages/{msg_id}/reaction`, `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`
- DB Table: `cpt-cf-mini-chat-dbtable-message-reactions`, `cpt-cf-mini-chat-dbtable-messages` (read only)
- Entities: MessageReaction, Message

### Temporary Chats Deferred

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-temporary-deferred`

The system **MUST NOT** implement temporary chats in P1 (`cpt-cf-mini-chat-fr-temporary-chat` is P2): no `POST /v1/chats/{id}:temporary` route is registered, no scheduled 24h deletion exists, `chats.is_temporary` is written only as `false` by the create insert and is never updated (create and rename ignore an `is_temporary` body field), and every `ChatDetail` returns `is_temporary` `false`.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-create-chat`
- `cpt-cf-mini-chat-flow-chat-crud-rename-chat`

**Touches**:
- DB Table: `cpt-cf-mini-chat-dbtable-chats` (`is_temporary`)
- Use case (P2, absent): `cpt-cf-mini-chat-usecase-temporary-chat-cleanup`

### REST Registration and OpenAPI

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-rest-routes`

The system **MUST** register, in the gear's REST registration step, the eight routes of the REST contract table under `url_prefix`, each requiring authentication and the license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1` (`cpt-cf-mini-chat-constraint-license-gate`), with OpenAPI metadata matching the generated document: request schemas `CreateChatReq`, `UpdateChatReq`, `SetReactionReq`; response schemas `ChatDetailDto`, `Page_ChatDetailDto`, `Page_MiniChatMessageDto` (`MiniChatMessageDto`, `AttachmentSummaryDto`, `ImgThumbnailDto`), `MiniChatReactionDto`; the `Location` header on the 201; query parameters `limit`, `cursor`, `$filter`, `$orderby` on both list routes (`$select` not declared); and the 400, 401, 403, 404 (routes with a path id), 422 (routes with a JSON body), 500 and 503 (`Retry-After`) problem responses (415 not declared). Handlers stay in the API layer and call domain services only; errors leave through `cpt-cf-mini-chat-algo-gear-foundation-map-error`.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-create-chat`
- `cpt-cf-mini-chat-flow-chat-crud-get-chat`
- `cpt-cf-mini-chat-flow-chat-crud-list-chats`
- `cpt-cf-mini-chat-flow-chat-crud-rename-chat`
- `cpt-cf-mini-chat-flow-chat-crud-delete-chat`
- `cpt-cf-mini-chat-flow-chat-crud-list-messages`
- `cpt-cf-mini-chat-flow-chat-crud-set-reaction`
- `cpt-cf-mini-chat-flow-chat-crud-remove-reaction`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: the eight routes above (`cpt-cf-mini-chat-interface-public-api`, `cpt-cf-mini-chat-interface-message-reaction`)

### Cross-Cutting Quality and Verification

- [x] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-crud-quality-gates`

The system **MUST** meet these cross-cutting requirements and completion signals:

- **Security**: every operation is authorized by the PEP with the action of the DESIGN §3.8 matrix and `resource.id` the chat id where present, fail-closed (deny 403 `AUTHZ_DENIED`, PDP failure 503 `Retry-After: 5`); every chat query is owner-scoped and every child query is tenant-scoped and filtered by the chat id of an owner-scoped chat query (`cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`, `cpt-cf-mini-chat-nfr-authz-alignment`); foreign resources are indistinguishable from missing ones (404); responses never contain `user_id`, `tenant_id`, provider identifiers or driver messages.
- **Reliability**: PDP evaluation bounded by 5 s and catalog reads by 10 s per plugin call and per registry query; the delete transaction is all-or-nothing, so a 204 always means the chat-cleanup message is durably stored and an error always means the chat is still active; concurrent deletes yield exactly one 204; concurrent reaction `PUT`s leave exactly one row.
- **Data integrity**: `chats.model` and `chats.is_temporary` are never updated; `title` is stored trimmed within 255 characters; reactions rely on UNIQUE `(message_id, user_id)` and the `reaction` CHECK; soft-deleted rows are never removed.
- **Observability**: no audit event and no metric instrument is added (none is defined for these operations); internal errors and the NULL `request_id` guard are logged with the operation and ids, never with message content or titles.
- **Rollback**: the feature adds no migration; rolling it back means removing its routes and services; soft-deleted chats stay soft-deleted and their enqueued cleanup messages stay in the outbox for the cleanup handler.
- **Test layering**: unit tests for title validation, model resolution (given, disabled, unknown, default flag, first enabled, empty catalog, plugin failure and timeout), pagination parameter handling, payload building and reaction value checks; integration tests on SQLite and PostgreSQL for every endpoint through the REST layer with a mock PDP and a static policy plugin, covering scoping, soft delete with the outbox row and `cleanup_status` changes, cursor walks without gaps or duplicates, and the message response contract; no live provider is called.
- **Compile-time gates**: domain code depends on repository ports, not on SeaORM entities; `make dylint` passes.
- **Completion signals**: automated tests exist and pass for every owned acceptance item listed in §1.4; `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass; every implemented CDSL instruction of this feature is wrapped in `@cpt-begin:{cpt-id}:p1:inst-{id}` / `@cpt-end:{cpt-id}:p1:inst-{id}` code markers (FULL traceability) and every flow, algorithm, state and definition-of-done ID of this feature appears in code; the checkboxes of this document and the featstatus are set to `[x]` only when their markers and tests exist.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-crud-create-chat`
- `cpt-cf-mini-chat-flow-chat-crud-delete-chat`
- `cpt-cf-mini-chat-algo-chat-crud-paginate`
- `cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- Build: `make gear-ci GEAR=mini-chat`, `make dylint`, `cfs validate`

## 6. Acceptance Criteria

- [ ] Principles & Constraints — "A chat's model is immutable once set": after `POST /v1/chats` with model `A`, `PATCH /v1/chats/{id}` with `{"title": "x", "model": "B"}` returns 200 with `model` `A`; `GET /v1/chats/{id}` and the list still return `A`; no route accepts a model change; disabling `A` in the catalog afterwards does not change `chats.model`.
- [ ] Chat CRUD — "Create / get / list / update / delete lifecycle for chats, including model and title validation": create returns 201 with exactly the `ChatDetail` keys, `message_count` 0, `is_temporary` `false`, no `title` key for an untitled chat and `Location` `{url_prefix}/v1/chats/{id}`; omitted `model` picks the first enabled `is_default` entry, else the first enabled entry; an unknown or disabled model, and an omitted model with no enabled entry, return 400 `INVALID_MODEL`; a title `""`, `"   "` or of 256 characters returns 400 `INVALID_TITLE` on create and rename, while a 255-character title and `"  Q3  "` (stored as `Q3`) succeed; a rename without `title` or with `title` `null` returns 422; get returns `message_count` and no messages; rename returns 200 with the new title and a later `updated_at`; delete returns 204, the chat then returns 404 on get, rename, delete, message list and reaction routes, and disappears from the list.
- [ ] Chat CRUD — "List endpoint supports filtering, ordering, and pagination, with validation of malformed input": `$filter` and `$orderby` work on `updated_at`, `id`, `title` (including `contains(title, '...')`); `limit` defaults to 20, `limit=500` returns `page_info.limit` 100, `limit=0` returns 400 `INVALID_LIMIT`; walking `next_cursor` returns every chat exactly once; an unknown field returns 400 `INVALID_FILTER` / `INVALID_ORDERBY_FIELD`, a malformed cursor 400 `INVALID_CURSOR`, `cursor` with `$orderby` 400 `ORDER_WITH_CURSOR`, `$skip` 400 `UNSUPPORTED_QUERY_PARAM`, each with `context.resource_type` `gts.cf.core.odata.query.v1~`; `$select` is accepted and the full item is returned.
- [ ] Chat CRUD — "Chat ordering reflects most recent activity": the default list order is `updated_at desc` then `id desc`; renaming an older chat moves it to the first position; chats created in the same instant keep a stable order across pages.
- [ ] Messages API — "List messages with filtering, ordering, and pagination": messages of a chat come in `created_at asc, id asc` order; `$filter` and `$orderby` on `created_at`, `id`, `role` work (for example `role eq 'assistant'`); default and maximum page size, clamping, `limit=0`, cursor walking and every malformed-input case behave as for the chat list; soft-deleted messages are never returned; a foreign or deleted chat returns 404 with the chat resource type.
- [ ] Messages API — "Message response contract: identity, attachments, and reaction fields are always consistently present": every item has `id`, `request_id` (non-null UUID, shared by the user and assistant message of one turn), `role`, `content`, `attachments` and `my_reaction`, `created_at`; `attachments` is `[]` when a message has none and lists only non-deleted attachments from `message_attachments` with `attachment_id`, `kind`, `filename`, `status` and `img_thumbnail` only for ready images; `my_reaction` is `null` for user and system messages and reflects only the caller's own reaction on assistant messages; `model` and token counts are omitted when absent or 0; no provider identifier appears; a stored NULL `request_id` yields 500.
- [ ] Reactions API — "Set/remove reaction on assistant messages only, idempotently": `PUT` `like` returns 200 `{message_id, reaction, created_at}` and the message list shows `my_reaction` `like`; `PUT` `dislike` replaces it with one row left; `PUT` `love` returns 400 `INVALID_REACTION` (also on a foreign chat, as it precedes authorization); a body without `reaction` returns 422; `PUT` or `DELETE` on a user message returns 400 `failed_precondition` with `violations[{subject: reaction_target, type: STATE}]`; `DELETE` returns 204 with and without an existing reaction; an unknown message returns 404 with `gts.cf.core.mini_chat.message.v1~`; another user's reaction is neither visible nor removable.
- [ ] Cleanup & Recovery (enqueue part, handler owned by `cpt-cf-mini-chat-feature-background-workers`): a delete writes, in the transaction that sets `chats.deleted_at`, exactly one outbox message on `mini-chat.chat_cleanup` with `tenant_id`, `chat_id`, `system_request_id`, `reason` `chat_soft_delete` and `chat_deleted_at` equal to `deleted_at`, and sets `cleanup_status` `pending` on the chat's attachments that had none; messages, turns, reactions and attachment `deleted_at` stay unchanged; an outbox failure leaves the chat active and returns an error; an oversized payload returns 400 with `context.format`.
- [ ] Authorization (operations of this feature): each endpoint sends its DESIGN §3.8 action (`create`, `list`, `read`, `update`, `delete`, `list_messages`, `set_reaction`, `delete_reaction`) with the chat id as `resource.id` where the matrix says so; PDP deny returns 403 `AUTHZ_DENIED`, PDP failure 503 with `Retry-After: 5`; a chat of another user or tenant returns 404 on every route, also when the PDP returns only a tenant predicate.
- [ ] Temporary chats deferred: no `:temporary` route exists; `is_temporary` is `false` on every chat and unchanged by create or rename bodies that carry it.
- [ ] Quality gates: `make gear-ci GEAR=mini-chat`, `make dylint` and `cfs validate` pass, and every implemented CDSL ID of this feature has `@cpt-begin` / `@cpt-end` markers.
