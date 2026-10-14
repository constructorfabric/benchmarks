---
description: "Specifies the Chat Management feature of the mini-chat gear: create, get, list, rename and delete of chats with model selection locked at creation, OData filtering, ordering and cursor pagination of the chat list, ordering by most recent activity, owner and tenant isolation, and the chat DELETE transaction that soft-deletes the chat and enqueues the chat-cleanup outbox message."
---

# Feature: Chat Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Chat User Creates a Chat](#chat-user-creates-a-chat)
  - [Chat User Gets a Chat](#chat-user-gets-a-chat)
  - [Chat User Lists Chats](#chat-user-lists-chats)
  - [Chat User Renames a Chat](#chat-user-renames-a-chat)
  - [Chat User Deletes a Chat](#chat-user-deletes-a-chat)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Title Validation](#title-validation)
  - [Chat Model Resolution](#chat-model-resolution)
  - [Model Immutability](#model-immutability)
  - [Message Count](#message-count)
  - [Chat Detail Projection](#chat-detail-projection)
  - [List Query Validation](#list-query-validation)
  - [Chat Activity Ordering](#chat-activity-ordering)
  - [Owner and Tenant Isolation of Chats](#owner-and-tenant-isolation-of-chats)
  - [Chat Delete Transaction](#chat-delete-transaction)
  - [Chat-Cleanup Enqueue Contract](#chat-cleanup-enqueue-contract)
- [4. States (CDSL)](#4-states-cdsl)
  - [Chat State Machine](#chat-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Create Chat With Title and Model Validation](#create-chat-with-title-and-model-validation)
  - [Get Chat With Message Count](#get-chat-with-message-count)
  - [List Chats With Filtering, Ordering and Pagination](#list-chats-with-filtering-ordering-and-pagination)
  - [Update Chat Title](#update-chat-title)
  - [Chat Delete Transaction and Chat-Cleanup Enqueue](#chat-delete-transaction-and-chat-cleanup-enqueue)
  - [Chat Ordering by Most Recent Activity](#chat-ordering-by-most-recent-activity)
  - [Model Immutability After Creation](#model-immutability-after-creation)
  - [Chat Isolation Enforced on All Five Operations](#chat-isolation-enforced-on-all-five-operations)
  - [Chat Routes Registered, Deferred Scope Unimplemented](#chat-routes-registered-deferred-scope-unimplemented)
  - [Test Layering, Traceability and Feature-Local Deviations](#test-layering-traceability-and-feature-local-deviations)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-featstatus-chat-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-mini-chat-feature-chat-management`

<!--
=============================================================================
FEATURE SPECIFICATION: Chat Management (mini-chat)
Source entry: DECOMPOSITION section 2.2 (gears/mini-chat/docs/DECOMPOSITION.md)
Precedence: gears/mini-chat/docs/ (PRD, DESIGN, ADR, acceptance-criteria) wins over
config/mini-chat.yaml and over platform documents under docs/ wherever they disagree.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

The Chat Management feature gives a user the five chat operations of the `mini-chat` gear: create, get, list, rename (update title) and delete. A chat is created with a model that is validated against the enabled catalog (or resolved as the default model) and stays locked to that model for its whole life. The chat list supports OData filtering and ordering and cursor pagination, ordered by most recent activity by default. Delete is one transaction that soft-deletes the chat, marks its attachments for provider cleanup and enqueues the chat-cleanup outbox message.

### 1.2 Purpose

A chat is the container that every conversation, attachment and turn lives in. This feature makes that container available and testable over plain REST without any provider call, on top of the schema, error contract, authorization PEP, scoped persistence, outbox enqueuer and model catalog rules delivered by `cpt-cf-mini-chat-feature-gear-foundation`. It realizes the chat CRUD and model selection requirements of the PRD and the "Model Locked Per Chat" constraint of the DESIGN.

Scope boundary: this feature owns the read and write behavior of the `chats` table and the synchronous first step of chat deletion. It does not own message content, streaming or the send pipeline, attachments, turn mutations, the writes that bump `chats.updated_at` for message activity (these belong to `cpt-cf-mini-chat-feature-message-streaming` and `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`), the `cleanup_status` state machine (`cpt-cf-mini-chat-feature-attachments-and-file-search`), or the chat-cleanup outbox handler and provider resource cleanup (`cpt-cf-mini-chat-feature-background-maintenance`).

**Requirements**: `cpt-cf-mini-chat-fr-chat-crud`, `cpt-cf-mini-chat-fr-model-selection`

**Requirements verified here but owned elsewhere**: `cpt-cf-mini-chat-nfr-tenant-isolation` (owned by `cpt-cf-mini-chat-feature-gear-foundation`, verified here through chat isolation tests), `cpt-cf-mini-chat-fr-chat-deletion-cleanup` and `cpt-cf-mini-chat-nfr-data-retention` (owned by `cpt-cf-mini-chat-feature-background-maintenance`; this feature provides only the soft-delete and enqueue step)

**Requirements tracked as deferred, not implemented in P1**: `cpt-cf-mini-chat-fr-temporary-chat` (P2), `cpt-cf-mini-chat-fr-group-chats` (P2+, ADR-0003)

**Principles verified here but owned by the foundation**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Design elements**: `cpt-cf-mini-chat-interface-public-api` (the five chat endpoints), `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments` (cleanup marking on delete), `cpt-cf-mini-chat-dbtable-messages` (message count source), `cpt-cf-mini-chat-component-chat-service`, `cpt-cf-mini-chat-component-chat-store`, `cpt-cf-mini-chat-design-authz-pep`, `cpt-cf-mini-chat-design-model-catalog` (default model and selection rules), `cpt-cf-mini-chat-seq-chat-deletion-cleanup` (first, synchronous step only)

**Use cases**: `cpt-cf-mini-chat-usecase-delete-chat` (single owner; its asynchronous cleanup step is owned by `cpt-cf-mini-chat-feature-background-maintenance`)

**Use cases tracked as deferred, not implemented in P1**: `cpt-cf-mini-chat-usecase-temporary-chat-cleanup` (P2)

**ADRs traced**: `cpt-cf-mini-chat-adr-canonical-error-contract` (error shapes), `cpt-cf-mini-chat-adr-data-lifecycle-audit-scope` (soft delete only, no purge, no deletion audit event), `cpt-cf-mini-chat-adr-group-chat-usage-attribution` (group chats deferred)

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-mini-chat-actor-chat-user` | Creates, reads, lists, renames and deletes their own chats through the public REST API under the gear URL prefix. Every call passes the license gate and the authorization PEP, and every chat is scoped to the calling user and tenant. |
| `cpt-cf-mini-chat-actor-cleanup-scheduler` | System actor that consumes the chat-cleanup outbox message enqueued by the chat DELETE transaction. Its handler is exercised in `cpt-cf-mini-chat-feature-background-maintenance`; this feature only produces the message. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) section 2.2 (`Chat Management`)
- **Acceptance criteria**: [acceptance-criteria.md](../acceptance-criteria.md)
- **ADRs (all accepted)**: [ADR-0003 group chat usage attribution](../ADR/0003-cpt-cf-mini-chat-adr-group-chat-usage-attribution.md), [ADR-0004 canonical error contract](../ADR/0004-cpt-cf-mini-chat-adr-canonical-error-contract.md), [ADR-0009 data lifecycle and audit scope](../ADR/0009-cpt-cf-mini-chat-adr-data-lifecycle-audit-scope.md)
- **Sibling feature**: [0001-gear-foundation.md](./0001-gear-foundation.md) (error mapping layer, authorization matrix and PEP, scoped persistence, outbox enqueuer, catalog rules, route registration, license gate)
- **Dependencies**: `cpt-cf-mini-chat-feature-gear-foundation`. Features that depend on this one: `cpt-cf-mini-chat-feature-message-streaming` (a turn needs an existing chat with a locked model) and `cpt-cf-mini-chat-feature-background-maintenance` (consumes the chat-cleanup message and acts on soft-deleted chats).

**Normative value sources** (the DESIGN holds the exact values; this document states the values it must preserve and names the DESIGN heading that carries the rest). The DESIGN names the generated OpenAPI (`docs/api/api.json` at the repository root) as the reference for request and response schemas; the operations are `mini_chat.create_chat`, `mini_chat.list_chats`, `mini_chat.get_chat`, `mini_chat.update_chat` and `mini_chat.delete_chat`, and the schemas are `CreateChatReq`, `UpdateChatReq`, `ChatDetailDto`, `Page_ChatDetailDto` and `PageInfo`.

| Topic | DESIGN location (section and heading) |
|-------|---------------------------------------|
| Endpoint overview, URL prefix | 3.3 API Contracts, `Endpoints Overview` |
| Create, list, get, update and delete contracts, title rules, list query parameters, 201 and `Location`, 204 | 3.3 API Contracts, the paragraphs `Create Chat`, `List Chats`, `Get Chat`, `Update Chat Title` and `Delete Chat` |
| Error conditions, statuses and reasons (`INVALID_TITLE`, `INVALID_MODEL`, OData query reasons, JSON body and path errors, outbox payload size) | 3.3 API Contracts, `Error Codes` (under `Provider Event Translation`) |
| Model locked per chat, selected and effective model | 2.2 Constraints, `Model Locked Per Chat` |
| Default model and selection validation | 4 Additional Context, `Model Catalog Configuration` (`Rules`); PRD `cpt-cf-mini-chat-fr-model-selection` |
| `chats` columns, indexes and Secure ORM declaration | 3.7 Database Schemas & Tables, `Table: chats` |
| Message rows counted for `message_count` | 3.7 Database Schemas & Tables, `Table: messages`; 3.3 `Get Chat` |
| Attachment cleanup columns marked on delete | 3.7 Database Schemas & Tables, `Table: attachments`; 3.6 `Attachment cleanup state machine` |
| Authorization matrix rows for the five operations and the owner predicate | 3.8 Authorization (PEP), `Per-Operation Authorization Matrix` |
| Delete transaction and chat-cleanup payload | 3.6 Interactions & Sequences, `Cleanup on Chat Deletion` and `Outbox payload and execution semantics`; 4 Additional Context, `Cleanup on Chat Deletion` |
| P1 delete behavior (soft delete only, no purge, no deletion audit event, running turn not cancelled) | ADR-0009; PRD `cpt-cf-mini-chat-fr-chat-deletion-cleanup` (`P1 status`) |

## 2. Actor Flows (CDSL)

User-facing interactions that start with the chat user and describe the end-to-end behavior of the five chat operations. All paths are served under the gear URL prefix (`url_prefix`, default `/mini-chat`). Authentication and the license gate run in api-gateway before every handler and extractor.

**Use cases**: `cpt-cf-mini-chat-usecase-delete-chat` is implemented by the delete flow below (synchronous steps 1 to 3 of the use case; step 4 belongs to `cpt-cf-mini-chat-feature-background-maintenance`, and step 5 is not implemented because chat deletion is not audited, ADR-0009). The use case `cpt-cf-mini-chat-usecase-temporary-chat-cleanup` is deferred and not implemented in P1.

### Chat User Creates a Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-management-create-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user sends an optional `title` and an optional `model` and receives HTTP 201 with the `ChatDetail` of the new chat (`message_count` equal to `0`, `is_temporary` equal to `false`, `model` equal to the resolved model) and a `Location` header `/mini-chat/v1/chats/{id}`.
- A body with neither field creates an untitled chat on the default model.

**Error Scenarios**:
- 401 `unauthenticated`, 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED`, 403 with `AUTHZ_DENIED`, 503 with `Retry-After: 5` (admission outcomes of the foundation).
- 415 for a JSON body without a JSON content type, 400 for malformed JSON, 422 for a body that does not match the schema.
- 400 `invalid_argument` with `field_violations[title].reason` equal to `INVALID_TITLE`.
- 400 `invalid_argument` with `field_violations[model].reason` equal to `INVALID_MODEL` for an unknown or disabled model, or when no enabled model exists and none was supplied.
- 500 `internal` when the model-policy plugin cannot be resolved or fails.

**Steps**:
1. [x] - `p1` - Chat User sends `POST /v1/chats` with a bearer token and a JSON body holding an optional `title` and an optional `model` - `inst-create-request`
2. [x] - `p1` - **IF** the bearer token is missing, invalid or expired (api-gateway authentication) - `inst-create-authn-failed`
   1. [x] - `p1` - **RETURN** 401 `unauthenticated` - `inst-create-return-401`
3. [x] - `p1` - Apply the license gate (license gate algorithm of the foundation) - `inst-create-license`
4. [x] - `p1` - **IF** the tenant lacks the required license feature - `inst-create-unlicensed`
   1. [x] - `p1` - **RETURN** 403 `permission_denied` with `LICENSE_FEATURE_REQUIRED` - `inst-create-return-license-403`
5. [x] - `p1` - Read the body through the platform JSON extractor - `inst-create-extract-body`
6. [x] - `p1` - **IF** the request carries a body without a JSON `Content-Type` - `inst-create-no-content-type`
   1. [x] - `p1` - **RETURN** 415 `invalid_argument` with `field_violations[body].reason` equal to `missing_json_content_type` - `inst-create-return-415`
7. [x] - `p1` - **IF** the body is malformed JSON - `inst-create-bad-json`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[body].reason` equal to `json_syntax_error` - `inst-create-return-400-json`
8. [x] - `p1` - **IF** the body does not match the schema (for example `title` or `model` is neither a string nor `null`) - `inst-create-schema-mismatch`
   1. [x] - `p1` - **RETURN** 422 `invalid_argument` with `field_violations[body].reason` equal to `invalid_json_body` - `inst-create-return-422`
9. [x] - `p1` - Validate the title (title validation algorithm), before the authorization check and before the model lookup - `inst-create-validate-title`
10. [x] - `p1` - **IF** the title is invalid - `inst-create-title-invalid`
    1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[title].reason` equal to `INVALID_TITLE` - `inst-create-return-400-title`
11. [x] - `p1` - Resolve the operation `POST /v1/chats` through the authorization operation matrix (action `create`, Chat type, no `resource.id`, `require_constraints` true) and evaluate it through the PEP authorization algorithm; the owner properties come from the security context - `inst-create-authorize`
12. [x] - `p1` - **IF** authorization returned a refusal (403 `AUTHZ_DENIED`, 403 for an unusable decision, or 503 with `Retry-After: 5`) - `inst-create-authz-refused`
    1. [x] - `p1` - **RETURN** the refusal Problem unchanged - `inst-create-return-refusal`
13. [x] - `p1` - Resolve the chat model (model resolution algorithm) from the optional `model` and the policy snapshot - `inst-create-resolve-model`
14. [x] - `p1` - **IF** model resolution failed - `inst-create-model-failed`
    1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[model].reason` equal to `INVALID_MODEL`, or 500 `internal` when the policy plugin failed - `inst-create-return-model-error`
15. [x] - `p1` - DB: INSERT `chats` through the scoped persistence algorithm with a new `id`, `tenant_id` and `user_id` taken from the security context (never from the body), the resolved `model`, the validated `title` or no title, `is_temporary` equal to `false`, `created_at` and `updated_at` set to the current time, and `deleted_at` empty - `inst-create-insert`
16. [x] - `p1` - Build the `ChatDetail` of the new chat with `message_count` equal to `0` (chat detail projection algorithm) - `inst-create-project`
17. [x] - `p1` - **RETURN** 201 with the `ChatDetail` body and the `Location` header `/mini-chat/v1/chats/{id}` (the request path as the gear sees it plus the new id, without the api-gateway `prefix_path`) - `inst-create-return-201`

### Chat User Gets a Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-management-get-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with the `ChatDetail` of their own chat: metadata and `message_count`, no embedded messages.

**Error Scenarios**:
- 404 `not_found` with `context.resource_type` equal to `gts.cf.core.mini_chat.chat.v1~` for a missing, soft-deleted, foreign-owner or foreign-tenant chat.
- 400 `invalid_argument` with `field_violations[].reason` equal to `invalid_path_params` for a path id that is not a UUID.
- 401, 403 and 503 admission refusals.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/chats/{id}` with a bearer token - `inst-get-request`
2. [x] - `p1` - **IF** the path `id` is not a UUID (platform path extractor, after the api-gateway authentication and license middleware) - `inst-get-bad-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` equal to `invalid_path_params` - `inst-get-return-400-path`
3. [x] - `p1` - Run the request admission flow of the foundation for the operation `GET /v1/chats/{id}` (action `read`, `resource.id` present), which authorizes the call and loads the chat through an owner-scoped query - `inst-get-admission`
4. [x] - `p1` - **IF** admission returned a refusal or the scoped query found no chat - `inst-get-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem (401, 403, 503) or 404 `not_found` for the chat resource type - `inst-get-return-refusal`
5. [x] - `p1` - Derive `message_count` for the chat (message count algorithm) - `inst-get-count`
6. [x] - `p1` - Build the `ChatDetail` (chat detail projection algorithm) - `inst-get-project`
7. [x] - `p1` - **RETURN** 200 with the `ChatDetail` - `inst-get-return`

### Chat User Lists Chats

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-management-list-chats`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 200 with `items` (the non-deleted chats of the caller, each a `ChatDetail`) and `page_info` with `limit`, `next_cursor` and `prev_cursor`, ordered by `updated_at` descending with `id` as tiebreaker unless `$orderby` or `cursor` says otherwise.
- A user without chats receives an empty `items` array.

**Error Scenarios**:
- 400 `invalid_argument` with `context.resource_type` equal to `gts.cf.core.odata.query.v1~` and a reason in `field_violations[].reason` for an unknown field, an operator outside the allowed set, a malformed filter, order, cursor, limit or select, an unsupported query option, or an unparsable query string.
- 401, 403 and 503 admission refusals.

**Steps**:
1. [x] - `p1` - Chat User sends `GET /v1/chats` with a bearer token and the optional query parameters `limit`, `cursor`, `$filter`, `$orderby` and `$select` - `inst-list-request`
2. [x] - `p1` - Run the request admission flow of the foundation for the operation `GET /v1/chats` (action `list`, no `resource.id`, constraints required), which returns an `AccessScope` limited to the caller's tenant and user - `inst-list-admission`
3. [x] - `p1` - **IF** admission returned a refusal - `inst-list-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem (401, 403, 503) - `inst-list-return-refusal`
4. [x] - `p1` - Validate and normalize the query (list query validation algorithm) - `inst-list-validate-query`
5. [x] - `p1` - **IF** the query is invalid - `inst-list-query-invalid`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `context.resource_type` equal to `gts.cf.core.odata.query.v1~` and the reason from the validation algorithm - `inst-list-return-400`
6. [x] - `p1` - DB: SELECT `chats` constrained by the `AccessScope` (tenant and owner) and `deleted_at IS NULL`, combined with the validated `$filter`, in the validated order, one page of the effective `limit` starting after the cursor position - `inst-list-select`
7. [x] - `p1` - **IF** the cursor does not match the query or cannot be decoded during pagination - `inst-list-cursor-failed`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with the reason `ORDER_MISMATCH`, `FILTER_MISMATCH` or `INVALID_CURSOR` - `inst-list-return-cursor-error`
8. [x] - `p1` - **FOR EACH** chat of the page - `inst-list-each-chat`
   1. [x] - `p1` - Derive `message_count` (message count algorithm) and build the `ChatDetail` (chat detail projection algorithm) - `inst-list-project-chat`
9. [x] - `p1` - Build `page_info` with the effective `limit` and the `next_cursor` and `prev_cursor` values (`null` when there is no such page) - `inst-list-page-info`
10. [x] - `p1` - **RETURN** 200 with `items` and `page_info` - `inst-list-return`

### Chat User Renames a Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-management-update-chat-title`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user sends `{title}` and receives HTTP 200 with the updated `ChatDetail`: the trimmed title, `updated_at` set to the current time, and `model`, `is_temporary` and `message_count` unchanged.
- A body such as a `title` plus a `model` returns 200, renames the chat and leaves `model` unchanged, because unknown fields are ignored.

**Error Scenarios**:
- 422 `invalid_argument` with `field_violations[body].reason` equal to `invalid_json_body` when `title` is missing, `null` or not a string; 400 `json_syntax_error` for malformed JSON; 415 `missing_json_content_type` without a JSON content type.
- 400 `invalid_argument` with `field_violations[title].reason` equal to `INVALID_TITLE` for an empty, whitespace-only or longer than 255 characters title.
- 404 `not_found` for a missing, soft-deleted, foreign-owner or foreign-tenant chat; 400 `invalid_path_params` for a non-UUID id; 401, 403 and 503 admission refusals.

**Steps**:
1. [x] - `p1` - Chat User sends `PATCH /v1/chats/{id}` with a bearer token and a JSON body holding `title` - `inst-update-request`
2. [x] - `p1` - **IF** the path `id` is not a UUID (after the api-gateway authentication and license middleware) - `inst-update-bad-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` equal to `invalid_path_params` - `inst-update-return-400-path`
3. [x] - `p1` - Read the body through the platform JSON extractor, which answers 415 `missing_json_content_type` for a body without a JSON content type, 400 `json_syntax_error` for malformed JSON and 422 `invalid_json_body` for a missing, `null` or non-string `title` - `inst-update-extract-body`
4. [x] - `p1` - **IF** the extractor rejected the body - `inst-update-body-rejected`
   1. [x] - `p1` - **RETURN** the 415, 400 or 422 Problem - `inst-update-return-body-error`
5. [x] - `p1` - Validate the title (title validation algorithm); as for create, the validation outcome does not depend on the chat, so it is evaluated before the chat is loaded - `inst-update-validate-title`
6. [x] - `p1` - **IF** the title is invalid - `inst-update-title-invalid`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[title].reason` equal to `INVALID_TITLE` - `inst-update-return-400-title`
7. [x] - `p1` - Run the request admission flow of the foundation for the operation `PATCH /v1/chats/{id}` (action `update`, `resource.id` present), which authorizes the call and loads the chat through an owner-scoped query - `inst-update-admission`
8. [x] - `p1` - **IF** admission returned a refusal or the scoped query found no chat - `inst-update-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem (401, 403, 503) or 404 `not_found` for the chat resource type - `inst-update-return-refusal`
9. [x] - `p1` - Apply the model immutability rules (model immutability algorithm): ignore every body field other than `title` - `inst-update-ignore-fields`
10. [x] - `p1` - DB: UPDATE `chats` set `title` to the trimmed title and `updated_at` to the current time, constrained by the `AccessScope`, the chat `id` and `deleted_at IS NULL`; no other column and no row of `messages` or `attachments` is written - `inst-update-write`
11. [x] - `p1` - **IF** the update affected no row (the chat was soft-deleted concurrently) - `inst-update-no-row`
    1. [x] - `p1` - **RETURN** 404 `not_found` for the chat resource type - `inst-update-return-404`
12. [x] - `p1` - Derive `message_count` and build the `ChatDetail` (message count and chat detail projection algorithms) - `inst-update-project`
13. [x] - `p1` - **RETURN** 200 with the updated `ChatDetail` - `inst-update-return`

### Chat User Deletes a Chat

- [x] `p1` - **ID**: `cpt-cf-mini-chat-flow-chat-management-delete-chat`

**Actor**: `cpt-cf-mini-chat-actor-chat-user`

**Success Scenarios**:
- The user receives HTTP 204 with no body. The chat no longer appears in get or list, a second `DELETE` returns 404, and provider cleanup proceeds asynchronously without blocking the response.

**Error Scenarios**:
- 404 `not_found` for a missing, already deleted, foreign-owner or foreign-tenant chat; 400 `invalid_path_params` for a non-UUID id; 401, 403 and 503 admission refusals.
- 400 `invalid_argument` (the message in `detail` and in `context.format`) when the chat-cleanup outbox payload exceeds the outbox size limit; the transaction rolls back and the chat stays active.
- 500 `internal` for a database or other unclassified failure; the transaction rolls back.

**Steps**:
1. [x] - `p1` - Chat User sends `DELETE /v1/chats/{id}` with a bearer token - `inst-delete-request`
2. [x] - `p1` - **IF** the path `id` is not a UUID (after the api-gateway authentication and license middleware) - `inst-delete-bad-path`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[].reason` equal to `invalid_path_params` - `inst-delete-return-400-path`
3. [x] - `p1` - Run the request admission flow of the foundation for the operation `DELETE /v1/chats/{id}` (action `delete`, `resource.id` present), which authorizes the call and loads the chat through an owner-scoped query - `inst-delete-admission`
4. [x] - `p1` - **IF** admission returned a refusal or the scoped query found no chat - `inst-delete-refused`
   1. [x] - `p1` - **RETURN** the refusal Problem (401, 403, 503) or 404 `not_found` for the chat resource type - `inst-delete-return-refusal`
5. [x] - `p1` - Run the delete transaction (chat delete transaction algorithm) - `inst-delete-transaction`
6. [x] - `p1` - **IF** the transaction reported that the chat was already deleted - `inst-delete-lost-race`
   1. [x] - `p1` - **RETURN** 404 `not_found` for the chat resource type, with no message enqueued - `inst-delete-return-404`
7. [x] - `p1` - **IF** the transaction reported an oversized chat-cleanup payload - `inst-delete-payload-too-large`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with the message in `detail` and in `context.format`, after the rollback - `inst-delete-return-400-payload`
8. [x] - `p1` - **IF** the transaction failed for any other reason - `inst-delete-failed`
   1. [x] - `p1` - Log the cause and **RETURN** 500 `internal` with a generic `detail` - `inst-delete-return-500`
9. [x] - `p1` - Leave a running turn of the chat untouched: it is not cancelled, is finalized normally and its usage is billed; emit no audit event for the deletion - `inst-delete-no-side-effects`
10. [x] - `p1` - **RETURN** 204 without waiting for provider cleanup; the chat-cleanup message is delivered later by the shared outbox to the handler of `cpt-cf-mini-chat-feature-background-maintenance` - `inst-delete-return-204`

## 3. Processes / Business Logic (CDSL)

Internal functions of the feature. The flows above call them, and the writers of later features reuse the activity ordering contract.

### Title Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-title-validation`

**Input**: the raw `title` of a create request (optional, may be absent or `null`) or of an update request (a non-null string, guaranteed by the JSON extractor)

**Output**: the stored title (trimmed string) or no title, or the error `INVALID_TITLE`. The rule is normative in DESIGN 3.3 (`Create Chat`, `Update Chat Title`, `Error Codes`).

**Steps**:
1. [x] - `p1` - **IF** the operation is create and `title` is absent or `null` - `inst-title-absent`
   1. [x] - `p1` - **RETURN** no title (an untitled chat; `chats.title` stays empty) - `inst-title-return-none`
2. [x] - `p1` - Trim leading and trailing whitespace from `title` - `inst-title-trim`
3. [x] - `p1` - Count the characters of the trimmed value - `inst-title-count`
4. [x] - `p1` - **IF** the trimmed value has fewer than 1 character (empty or whitespace-only) or more than 255 characters - `inst-title-out-of-range`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[title].reason` equal to `INVALID_TITLE` - `inst-title-return-error`
5. [x] - `p1` - **RETURN** the trimmed value as the title to store; the response `title` is the trimmed value - `inst-title-return`

### Chat Model Resolution

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-model-resolution`

**Input**: the optional `model` string of a create request and the policy snapshot read through the model-policy gateway

**Output**: the model identifier to store in `chats.model`, or `INVALID_MODEL`, or a plugin error. The catalog rules are provided by the foundation (catalog rules algorithm); the rule is normative in DESIGN 4 `Model Catalog Configuration` (`Rules`) and PRD `cpt-cf-mini-chat-fr-model-selection`.

**Steps**:
1. [x] - `p1` - Read the policy snapshot through the policy snapshot access algorithm of the foundation (no local cache) - `inst-model-read-snapshot`
2. [x] - `p1` - **IF** the snapshot could not be read - `inst-model-snapshot-failed`
   1. [x] - `p1` - **RETURN** a plugin error that the caller maps to 500 `internal` - `inst-model-return-plugin-error`
3. [x] - `p1` - **IF** the request supplied a `model` - `inst-model-supplied`
   1. [x] - `p1` - Look up the catalog entry whose `id` equals the supplied value; an entry that omits `enabled` counts as disabled - `inst-model-lookup`
   2. [x] - `p1` - **IF** no entry exists or the entry is disabled - `inst-model-not-selectable`
      1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[model].reason` equal to `INVALID_MODEL` - `inst-model-return-invalid-supplied`
   3. [x] - `p1` - **RETURN** the supplied `model` as the model to store - `inst-model-return-supplied`
4. [x] - `p1` - Resolve the default model: the first enabled entry with `preference.is_default` true, else the first enabled entry, in catalog order and without considering the tier - `inst-model-default`
5. [x] - `p1` - **IF** the catalog has no enabled entry - `inst-model-none-enabled`
   1. [x] - `p1` - **RETURN** 400 `invalid_argument` with `field_violations[model].reason` equal to `INVALID_MODEL` - `inst-model-return-invalid-default`
6. [x] - `p1` - **RETURN** the `id` of the default entry as the model to store - `inst-model-return-default`

### Model Immutability

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-model-immutability`

**Input**: any chat operation on an existing chat (get, list, update, delete, and the writes of later features)

**Output**: the guarantee that `chats.model` (the selected model) never changes after the create insert. The constraint is `cpt-cf-mini-chat-constraint-model-locked-per-chat` (DESIGN 2.2 `Model Locked Per Chat`).

**Steps**:
1. [x] - `p1` - Write `chats.model` only in the create insert; register no endpoint and add no repository write that changes `model` afterwards (the user who wants another model creates a new chat) - `inst-immutable-single-write`
2. [x] - `p1` - In the update operation write only `title` and `updated_at`, and ignore every other body field including `model` and `is_temporary`; the response still carries the stored `model` - `inst-immutable-patch`
3. [x] - `p1` - Return the stored `chats.model` unchanged on get, list and update, without a catalog lookup, so a chat whose model was later disabled or removed from the catalog is still readable, renameable and deletable; the rejection with `INVALID_MODEL` for such a chat belongs to the send, retry, edit and upload operations of later features - `inst-immutable-reads`
4. [x] - `p1` - Never write `chats.model` for a quota-driven downgrade; the effective model of a turn is recorded on the assistant message by the features that own the turn - `inst-immutable-downgrade`
5. [x] - `p1` - **RETURN** the unchanged selected model - `inst-immutable-return`

### Message Count

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-message-count`

**Input**: a chat id obtained from an owner-scoped chat query, and the tenant id

**Output**: the non-negative integer `message_count` of the chat. The `chats` table has no count column (DESIGN 3.7 `Table: chats`); the field is part of the `ChatDetail` read contract (DESIGN 3.3 `Get Chat`).

**Steps**:
1. [x] - `p1` - DB: count the `messages` rows whose `chat_id` is the given chat id and whose `tenant_id` is the caller's tenant and whose `deleted_at IS NULL`, all roles included, which is the set that `GET /v1/chats/{id}/messages` lists - `inst-count-query`
2. [x] - `p1` - Reach `messages` only through the chat id of the owner-scoped chat query (child tables are never read by their own id alone) - `inst-count-scoped`
3. [x] - `p1` - Leave the count unaffected by rename and by reads: updating the title does not touch messages - `inst-count-stable`
4. [x] - `p1` - **RETURN** the count (`0` for a new chat); messages written by later features are counted through the same rule - `inst-count-return`

### Chat Detail Projection

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-chat-detail-projection`

**Input**: a chat row and its `message_count`

**Output**: the `ChatDetail` object shared by create, get, update and every list item (`ChatDetailDto` in the reference OpenAPI).

**Steps**:
1. [x] - `p1` - Emit the required fields `id` (UUID), `model` (string, the stored selected model), `is_temporary` (boolean, always `false` in P1), `message_count` (int64), `created_at` and `updated_at` (RFC 3339 date-time) - `inst-projection-required`
2. [x] - `p1` - Emit `title` only when the chat has one; omit the field from the JSON when the chat is untitled and never send it as `null` (the OpenAPI schema permits `null`, the DESIGN contract is omission) - `inst-projection-title`
3. [x] - `p1` - Never emit `user_id`, `tenant_id`, `deleted_at` or any provider identifier; identity comes from the authentication context - `inst-projection-no-internal`
4. [x] - `p1` - Embed no messages: history is loaded through `GET /v1/chats/{id}/messages` - `inst-projection-no-messages`
5. [x] - `p1` - **RETURN** the `ChatDetail` - `inst-projection-return`

### List Query Validation

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-list-query`

**Input**: the query parameters of `GET /v1/chats`

**Output**: a validated filter, order, effective page size and cursor, or a 400 `invalid_argument` whose `context.resource_type` is `gts.cf.core.odata.query.v1~` and whose reason is in `field_violations[].reason`. The rules are normative in DESIGN 3.3 (`List Chats`, `Error Codes`) and in the reference OpenAPI (`x-odata-filter`, `x-odata-orderby`); the reason codes come from the platform OData library and extractor that the foundation error mapping carries.

**Steps**:
1. [x] - `p1` - **IF** the query string cannot be parsed - `inst-query-unparsable`
   1. [x] - `p1` - **RETURN** reason `INVALID_QUERY_PARAMS` - `inst-query-return-params`
2. [x] - `p1` - **IF** the query carries a `$` option other than `$filter`, `$orderby` and `$select` (for example `$skip` or `$count`) - `inst-query-unsupported`
   1. [x] - `p1` - **RETURN** reason `UNSUPPORTED_QUERY_PARAM` - `inst-query-return-unsupported`
3. [x] - `p1` - Validate the syntax of `$select`: a bad `$select` returns reason `INVALID_SELECT`; a valid `$select` is ignored and the full item is always returned (it is not declared in the OpenAPI document) - `inst-query-select`
4. [x] - `p1` - Resolve the page size from `limit` (the platform extractor binds `limit` as an unsigned 64-bit integer and also accepts the alias `$top`; the outcomes are taken from the platform OData extractor `libs/toolkit/src/api/odata.rs` and `libs/toolkit-odata/src/problem_mapping.rs`, and DESIGN 3.3 `Error Codes` row `Bad OData query on a list endpoint` for `INVALID_LIMIT` and `INVALID_QUERY_PARAMS`) - `inst-query-limit`
   1. [x] - `p1` - **IF** `limit` is absent - **RETURN** effective page size `20` (HTTP 200, no error) - `inst-query-limit-absent`
   2. [x] - `p1` - **IF** `limit` is an integer from `1` to `100` (for example `1` or `100`) - **RETURN** that value as the effective page size (HTTP 200) - `inst-query-limit-valid`
   3. [x] - `p1` - **IF** `limit` is an integer above `100` and up to `18446744073709551615` (for example `101`) - **RETURN** effective page size `100` (HTTP 200, clamped, not an error) - `inst-query-limit-clamp`
   4. [x] - `p1` - **IF** `limit` is `0` - **RETURN** HTTP 400 `invalid_argument` with reason `INVALID_LIMIT` and field `$top` (the extractor keys the violation to `$top` even when the caller sent `limit`) - `inst-query-limit-zero`
   5. [x] - `p1` - **IF** `limit` is not an unsigned integer that fits in 64 bits (negative such as `-1`, non-numeric such as `abc`, fractional such as `1.5`, empty, or larger than `18446744073709551615`) - **RETURN** HTTP 400 `invalid_argument` with reason `INVALID_QUERY_PARAMS` and field `query`; the request is rejected when the query string is bound, so the reason is not `INVALID_LIMIT` and the value is never clamped - `inst-query-limit-unparsable`
   6. [x] - `p1` - **IF** both `limit` and `$top` are present in one request - **RETURN** HTTP 400 `invalid_argument` with reason `INVALID_QUERY_PARAMS` and field `query` (duplicate field) - `inst-query-limit-duplicate`
5. [x] - `p1` - Validate `$filter`: the allowed fields and operators are `updated_at` with `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `in`; `id` with `eq`, `ne`, `in`; `title` with `eq`, `ne`, `contains`, `startswith`, `endswith`, `in`; an unknown field, an operator outside the field's set or a malformed expression returns reason `INVALID_FILTER`, a filter above the length limit returns `FILTER_TOO_LONG` and one above the complexity limit returns `FILTER_TOO_COMPLEX` - `inst-query-filter`
6. [x] - `p1` - Validate `$orderby`: the allowed values are `updated_at asc`, `updated_at desc`, `id asc`, `id desc`, `title asc` and `title desc`; any other field or direction returns reason `INVALID_ORDERBY_FIELD` - `inst-query-orderby`
7. [x] - `p1` - **IF** neither `$orderby` nor `cursor` is given - `inst-query-default-order`
   1. [x] - `p1` - Use the default order `updated_at desc` (most recently active first) with `id` as tiebreaker - `inst-query-apply-default-order`
8. [x] - `p1` - **IF** `cursor` is given together with `$orderby` - `inst-query-cursor-with-order`
   1. [x] - `p1` - **RETURN** reason `ORDER_WITH_CURSOR` (a cursor carries its own order) - `inst-query-return-order-with-cursor`
9. [x] - `p1` - Treat `cursor` as opaque: a cursor that cannot be decoded returns reason `INVALID_CURSOR`; a cursor issued for another order returns `ORDER_MISMATCH` and one issued for another filter returns `FILTER_MISMATCH`; the cursor serves both next and previous page navigation - `inst-query-cursor`
10. [x] - `p1` - **RETURN** the validated query, or the first violation found as 400 `invalid_argument` - `inst-query-return`

### Chat Activity Ordering

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-activity-ordering`

**Input**: a write that counts as chat activity, and the chat list read

**Output**: `chats.updated_at` as the single activity timestamp, and a list order that reflects the most recent activity. The rule is normative in DESIGN 3.3 (`List Chats`) and 3.7 (`Table: chats`, `updated_at`).

**Steps**:
1. [x] - `p1` - Set `updated_at` to the current time at chat creation (create insert) - `inst-activity-create`
2. [x] - `p1` - Set `updated_at` to the current time at rename, in the same UPDATE that writes the title - `inst-activity-rename`
3. [x] - `p1` - Set `updated_at` to the deletion time at delete, in the same UPDATE that sets `deleted_at` - `inst-activity-delete`
4. [x] - `p1` - Define the contract for message activity: a sent message, a retry and an edit each move `updated_at` forward to the current time inside the same database transaction as the message write, never in a separate transaction; those writes belong to `cpt-cf-mini-chat-feature-message-streaming` and `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` and are exercised there, and this feature verifies the contract with direct repository writes made under the same rule - `inst-activity-message-contract`
5. [x] - `p1` - Leave `updated_at` unchanged on get and list (reads are not activity) - `inst-activity-reads`
6. [x] - `p1` - Order the list by `updated_at desc` with `id` as tiebreaker by default, served by the partial index `(tenant_id, user_id, updated_at DESC) WHERE deleted_at IS NULL` - `inst-activity-list-order`
7. [x] - `p1` - **RETURN** the activity timestamp contract - `inst-activity-return`

### Owner and Tenant Isolation of Chats

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-chat-isolation`

**Input**: any of the five chat operations and the caller's security context

**Output**: access only to the caller's own chats, or a refusal. The matrix rows are normative in DESIGN 3.8 (`Per-Operation Authorization Matrix`); the machinery belongs to the foundation (PEP authorization and scoped persistence algorithms).

**Steps**:
1. [x] - `p1` - Use the authorization matrix rows `create` (`POST /v1/chats`), `list` (`GET /v1/chats`), `read` (`GET /v1/chats/{id}`), `update` (`PATCH /v1/chats/{id}`) and `delete` (`DELETE /v1/chats/{id}`) on the Chat type, with `resource.id` on the three chat-addressed rows and constraints required on all five - `inst-isolation-matrix`
2. [x] - `p1` - Run every database access of the feature through the `AccessScope` that carries the tenant predicate and the owner predicate (the PEP adds the owner predicate even when the PDP returns only a tenant predicate) - `inst-isolation-scope`
3. [x] - `p1` - Take `tenant_id` and `user_id` of a new chat from the security context, never from the request body - `inst-isolation-create-owner`
4. [x] - `p1` - Answer 404 `not_found` with `context.resource_type` equal to `gts.cf.core.mini_chat.chat.v1~` for a missing, soft-deleted, foreign-owner or foreign-tenant chat, so the four cases are indistinguishable and a foreign chat is never reported as 403 - `inst-isolation-404`
5. [x] - `p1` - Return from list only chats of the caller, never another user's chats of the same tenant and never another tenant's chats, whatever filter or cursor the request carries - `inst-isolation-list`
6. [x] - `p1` - Treat the sub-resources of a deleted chat as unreachable: every read goes through the chat, so chat-addressed routes of later features answer 404 after the soft-delete without any child row being modified - `inst-isolation-children`
7. [x] - `p1` - **RETURN** the scoped access result - `inst-isolation-return`

### Chat Delete Transaction

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-delete-transaction`

**Input**: the chat loaded by the owner-scoped query (its `id` and `tenant_id`) and the `AccessScope`

**Output**: a committed soft-delete with marked attachments and one enqueued chat-cleanup message, or a rolled-back transaction with a reported reason. The transaction is normative in DESIGN 3.6 `Cleanup on Chat Deletion` and 4 `Cleanup on Chat Deletion`.

**Steps**:
1. [x] - `p1` - Open one database transaction through the scoped persistence layer; every step below runs in it - `inst-tx-begin`
2. [x] - `p1` - DB: UPDATE `chats` set `deleted_at` and `updated_at` to the same current time, constrained by the `AccessScope`, the chat `id` and `deleted_at IS NULL` - `inst-tx-soft-delete`
3. [x] - `p1` - **IF** the update affected no row (another delete committed first) - `inst-tx-already-deleted`
   1. [x] - `p1` - Roll back and **RETURN** the already-deleted result, which the flow maps to 404 - `inst-tx-return-already-deleted`
4. [x] - `p1` - DB: UPDATE `attachments` set `cleanup_status` to `pending` for the rows of this chat (filter `tenant_id` and `chat_id`) that have no cleanup status yet; rows already `pending`, `done` or `failed` keep their value because the only allowed transitions are `pending` to `done` and `pending` to `failed` (state machine owned by `cpt-cf-mini-chat-feature-attachments-and-file-search`); documents and images are both marked - `inst-tx-mark-attachments`
5. [x] - `p1` - Leave `messages`, `chat_turns`, `message_attachments`, `message_reactions` and the `deleted_at` of every attachment untouched: they become unreachable because every read goes through the chat - `inst-tx-children-untouched`
6. [x] - `p1` - Enqueue the chat-cleanup outbox message in this same transaction (chat-cleanup enqueue algorithm) - `inst-tx-enqueue`
7. [x] - `p1` - **IF** the enqueue reported a payload above the outbox size limit - `inst-tx-payload-too-large`
   1. [x] - `p1` - Roll back the whole transaction (the chat stays active, the attachments are not marked) and **RETURN** the oversized-payload result, which the flow maps to 400 `invalid_argument` - `inst-tx-return-payload-error`
8. [x] - `p1` - **TRY** - `inst-tx-try`
   1. [x] - `p1` - Commit the transaction, so the soft-delete, the attachment marking and the outbox message become visible together - `inst-tx-commit`
9. [x] - `p1` - **CATCH** a database failure - `inst-tx-catch`
   1. [x] - `p1` - Roll back, so none of the three writes survives, and **RETURN** the failure that the flow maps to 500 `internal` - `inst-tx-return-failure`
10. [x] - `p1` - Perform no provider call, no vector store call and no wait inside the request; provider files and the chat vector store are removed later by the chat-cleanup handler - `inst-tx-no-provider-call`
11. [x] - `p1` - **RETURN** the committed result - `inst-tx-return`

### Chat-Cleanup Enqueue Contract

- [x] `p1` - **ID**: `cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue`

**Input**: the `tenant_id` and `chat_id` of the chat, the `deleted_at` value written by the soft-delete, and the open delete transaction

**Output**: one durable message in the chat-cleanup queue (`outbox.chat_cleanup_queue_name`, default `mini-chat.chat_cleanup`), committed or rolled back with the transaction. The payload is normative in DESIGN 3.6 `Outbox payload and execution semantics`; the queue and the transactional enqueuer are provided by the foundation (outbox queues algorithm).

**Steps**:
1. [x] - `p1` - Generate a new server-side UUID v4 as the stable `system_request_id` and persist it in the payload at enqueue time, so that every retry, replay and dead-letter handoff of this message reuses it unchanged - `inst-enqueue-request-id`
2. [x] - `p1` - Serialize at least `tenant_id`, `chat_id`, `system_request_id`, `reason` equal to `chat_soft_delete`, and `chat_deleted_at` equal to the `deleted_at` written by the soft-delete - `inst-enqueue-payload`
3. [x] - `p1` - Enqueue through the foundation's transactional enqueuer on the chat-cleanup queue, partitioned by `chat_id`, inside the caller's transaction; any `dedupe_key` set on the message satisfies the foundation dedupe prefix rule (32-character lowercase hex `tenant_id` followed by `/`) - `inst-enqueue-transactional`
4. [x] - `p1` - Enqueue exactly one message per chat deletion, and only in the transaction that sets `deleted_at`, so the message never exists for an active chat and the handler's rejection of a not-soft-deleted chat is unreachable through this path - `inst-enqueue-once`
5. [x] - `p1` - **IF** the serialized payload exceeds the outbox payload size limit - `inst-enqueue-too-large`
   1. [x] - `p1` - **RETURN** the enqueue error that the delete flow maps to 400 `invalid_argument` (the same failure on attachment delete and on turn retry, edit and delete is 500 `internal`) - `inst-enqueue-return-error`
6. [x] - `p1` - Leave the consumption to the chat-cleanup handler of `cpt-cf-mini-chat-feature-background-maintenance`, which loads the current attachment rows and the `chat_vector_stores` row from `tenant_id` and `chat_id` of the payload - `inst-enqueue-consumer-boundary`
7. [x] - `p1` - **RETURN** the enqueued message - `inst-enqueue-return`

## 4. States (CDSL)

### Chat State Machine

- [x] `p2` - **ID**: `cpt-cf-mini-chat-state-chat-management-chat-lifecycle`

**States**: Active, Deleted

**Initial State**: Active

The selected model is fixed when the chat enters Active and never changes in any state (model immutability algorithm). `is_temporary` stays `false` in P1; the temporary-chat transition to Deleted after 24 hours is deferred to P2 and not implemented.

**Transitions**:
1. [x] - `p1` - **FROM** Active **TO** Active **WHEN** the owner renames the chat (the title changes and `updated_at` moves forward) - `inst-state-rename`
2. [x] - `p1` - **FROM** Active **TO** Active **WHEN** a sent message, retry or edit of a later feature bumps `updated_at` in its own transaction - `inst-state-activity`
3. [x] - `p1` - **FROM** Active **TO** Deleted **WHEN** the delete transaction commits (`deleted_at` is set, attachments are marked `pending` and the chat-cleanup message is enqueued) - `inst-state-delete`
4. [x] - `p1` - **FROM** Deleted **TO** Deleted **WHEN** any chat operation addresses the chat (the answer is 404 and nothing changes, no hard purge happens in P1, ADR-0009) - `inst-state-deleted-terminal`

## 5. Definitions of Done

Specific implementation tasks derived from the flows and algorithms above. Each is implemented and tested; every acceptance-criteria item of DECOMPOSITION 2.2 is mandatory.

### Create Chat With Title and Model Validation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-create`

The system **MUST** serve `POST /v1/chats` so that a valid request creates a chat owned by the caller and answers HTTP 201 with the `ChatDetail` and the `Location` header `/mini-chat/v1/chats/{id}` (declared on the 201 response in the OpenAPI), where the `id` is the new chat id.

- `title` is optional; absent or `null` creates an untitled chat; otherwise it is trimmed and must be 1 to 255 characters, else 400 `INVALID_TITLE`, evaluated before the authorization check and before the model lookup.
- `model` is optional; a supplied value must be an enabled catalog entry, else 400 `INVALID_MODEL`; an absent value resolves to the first enabled `is_default` entry, else the first enabled entry, regardless of tier; no enabled entry gives 400 `INVALID_MODEL`. Only create rejects a disabled model.
- JSON body errors map to 415 `missing_json_content_type`, 400 `json_syntax_error` and 422 `invalid_json_body`; admission refusals map to 401, 403 and 503; a policy plugin failure maps to 500.
- The reference OpenAPI also declares the throttling of the operation (`x-rate-limit-rps` 10, `x-rate-limit-burst` 10, `x-in-flight-limit` 100).

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-create-chat`
- `cpt-cf-mini-chat-algo-chat-management-title-validation`
- `cpt-cf-mini-chat-algo-chat-management-model-resolution`
- `cpt-cf-mini-chat-algo-chat-management-chat-detail-projection`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `POST /v1/chats`
- DB: `cpt-cf-mini-chat-dbtable-chats`
- Entities: `Chat`

### Get Chat With Message Count

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-get`

The system **MUST** serve `GET /v1/chats/{id}` so that the owner receives HTTP 200 with the `ChatDetail` (metadata and `message_count`, no embedded messages), with `title` omitted for an untitled chat, and receives 404 for any chat that is missing, soft-deleted or not theirs.

- `message_count` is derived at read time from the non-deleted `messages` rows of the chat (no stored counter on `chats`), is `0` for a new chat, is not changed by a rename, and counts messages written by later features without further change.
- A non-UUID path id gives 400 `invalid_path_params`; `user_id`, `tenant_id` and `deleted_at` never appear in the body.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-get-chat`
- `cpt-cf-mini-chat-algo-chat-management-message-count`
- `cpt-cf-mini-chat-algo-chat-management-chat-detail-projection`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `GET /v1/chats/{id}`
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages`
- Entities: `Chat`, `Message`

### List Chats With Filtering, Ordering and Pagination

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-list`

The system **MUST** serve `GET /v1/chats` as a cursor-paginated list of the caller's non-deleted chats, with `$filter` and `$orderby` over `updated_at`, `id` and `title`, and **MUST** reject malformed input with 400 `invalid_argument`.

- `limit` is resolved as follows (source: DESIGN 3.3 `List Chats` and `Error Codes` rows for `INVALID_LIMIT` and `INVALID_QUERY_PARAMS`, and the platform extractor `libs/toolkit/src/api/odata.rs` with `libs/toolkit-odata/src/problem_mapping.rs`): absent gives `20` (200); `1` gives `1` (200); `100` gives `100` (200); `101` and any larger value up to `18446744073709551615` give `100` (200, clamped); `0` gives 400 `INVALID_LIMIT` with field `$top`; `-1`, `abc`, `1.5`, an empty value and a value above `18446744073709551615` give 400 `INVALID_QUERY_PARAMS` with field `query`; `limit` together with `$top` gives 400 `INVALID_QUERY_PARAMS` with field `query`.
- The filter operators are those of the OpenAPI `x-odata-filter` (`updated_at`: `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `in`; `id`: `eq`, `ne`, `in`; `title`: `eq`, `ne`, `contains`, `startswith`, `endswith`, `in`), and the orders are those of `x-odata-orderby`; the default order is `updated_at desc` with `id` tiebreaker.
- Malformed input returns `context.resource_type` equal to `gts.cf.core.odata.query.v1~` and one of `INVALID_FILTER`, `INVALID_ORDERBY_FIELD`, `INVALID_LIMIT`, `INVALID_CURSOR`, `ORDER_MISMATCH`, `FILTER_MISMATCH`, `ORDER_WITH_CURSOR`, `FILTER_TOO_LONG`, `FILTER_TOO_COMPLEX`, `INVALID_SELECT`, `UNSUPPORTED_QUERY_PARAM`, `INVALID_QUERY_PARAMS`; `$select` is validated and ignored and is not declared in the OpenAPI.
- The response is `items` of `ChatDetail` plus `page_info` (`limit`, `next_cursor`, `prev_cursor`); only the caller's chats are returned even when the PDP returns only a tenant predicate.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-list-chats`
- `cpt-cf-mini-chat-algo-chat-management-list-query`
- `cpt-cf-mini-chat-algo-chat-management-chat-detail-projection`
- `cpt-cf-mini-chat-algo-chat-management-message-count`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `GET /v1/chats`
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-messages`
- Entities: `Chat`

### Update Chat Title

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-update`

The system **MUST** serve `PATCH /v1/chats/{id}` so that it updates only `title` (and `updated_at`) of the caller's chat and answers HTTP 200 with the updated `ChatDetail`.

- `title` must be a present, non-null string (else 422 `invalid_json_body`), trimmed to 1 to 255 characters (else 400 `INVALID_TITLE`); unknown body fields are ignored, so a body that also carries `model` renames the chat and leaves `model` unchanged.
- The update does not touch `model`, `is_temporary`, `message_count`, messages or attachments, and sets `updated_at` to the current time.
- A missing, soft-deleted or foreign chat gives 404; a chat soft-deleted between the load and the write gives 404.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-update-chat-title`
- `cpt-cf-mini-chat-algo-chat-management-title-validation`
- `cpt-cf-mini-chat-algo-chat-management-model-immutability`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `PATCH /v1/chats/{id}`
- DB: `cpt-cf-mini-chat-dbtable-chats`
- Entities: `Chat`

### Chat Delete Transaction and Chat-Cleanup Enqueue

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-delete`

The system **MUST** serve `DELETE /v1/chats/{id}` as one transaction that soft-deletes the chat, marks its attachments `cleanup_status = pending` and enqueues the chat-cleanup outbox message, and answers HTTP 204 without waiting for provider cleanup.

- After the commit the chat is absent from get and list, a second `DELETE` returns 404, and messages, turns, reactions and attachment rows keep `deleted_at` empty; the attachment rows of the chat have `cleanup_status` equal to `pending` unless a cleanup status was already set.
- The outbox message is in queue `outbox.chat_cleanup_queue_name` (default `mini-chat.chat_cleanup`), partitioned by `chat_id`, with the payload fields `tenant_id`, `chat_id`, a stable `system_request_id` (UUID v4 generated at enqueue), `reason` equal to `chat_soft_delete` and `chat_deleted_at`.
- The three writes commit or roll back together: an enqueue failure leaves the chat active and the attachments unmarked; an oversized payload gives 400 `invalid_argument` (the message in `detail` and `context.format`); another failure gives 500 `internal`.
- A running turn is not cancelled and is billed; no audit event is emitted for the deletion; no hard purge exists (ADR-0009). The handler that consumes the message belongs to `cpt-cf-mini-chat-feature-background-maintenance`, which the message contract of this feature must satisfy.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-delete-chat`
- `cpt-cf-mini-chat-algo-chat-management-delete-transaction`
- `cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `DELETE /v1/chats/{id}`
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments`, shared outbox tables (platform migrations)
- Entities: `Chat`, `Attachment`

### Chat Ordering by Most Recent Activity

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-activity-ordering`

The system **MUST** keep `chats.updated_at` as the activity timestamp and order the chat list by it (newest first, `id` tiebreaker) by default.

- Create, rename and delete set `updated_at`; reads do not change it.
- The message-activity contract (a sent message, a retry and an edit bump `updated_at` in the same transaction as the message write) is fixed here and exercised by the features that own those writes; this feature verifies it with direct repository writes that follow the same rule.
- A chat bumped after another appears before it in the default list, and an explicit `$orderby` overrides the default.

**Implements**:
- `cpt-cf-mini-chat-algo-chat-management-activity-ordering`
- `cpt-cf-mini-chat-flow-chat-management-list-chats`

**Constraints**: `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: `GET /v1/chats`
- DB: `cpt-cf-mini-chat-dbtable-chats`
- Entities: `Chat`

### Model Immutability After Creation

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-model-immutability`

The system **MUST** keep the chat model (`chats.model`, the selected model) unchanged for the whole life of the chat: written only at creation, never by the update operation, and never by a downgrade.

- No endpoint and no repository write changes `model` after the insert; `PATCH` ignores a `model` field.
- Get, list, update and delete do not consult the catalog, so a chat whose model was disabled or removed later is still served with its stored `model`.

**Implements**:
- `cpt-cf-mini-chat-algo-chat-management-model-immutability`
- `cpt-cf-mini-chat-flow-chat-management-update-chat-title`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`

**Touches**:
- API: `PATCH /v1/chats/{id}`, `GET /v1/chats/{id}`, `GET /v1/chats`
- DB: `cpt-cf-mini-chat-dbtable-chats`
- Entities: `Chat`

### Chat Isolation Enforced on All Five Operations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-isolation`

The system **MUST** scope all five chat operations to the owner and tenant of the caller through the foundation PEP and scoped persistence, and **MUST** answer 404 `not_found` (resource type `gts.cf.core.mini_chat.chat.v1~`) for any chat that is missing, soft-deleted or not the caller's.

- The authorization matrix rows used are `create`, `list`, `read`, `update` and `delete` of the Chat type with the `resource.id` presence and `require_constraints` values of DESIGN 3.8.
- A foreign chat is never reported as 403; a PDP deny is 403 `AUTHZ_DENIED`, a PDP failure is 503 with `Retry-After: 5`, and a decision without usable constraints is 403.
- Tests use two tenants and two users in one tenant, and prove that no read, list, update or delete crosses the owner or tenant boundary and that a deleted chat is unreachable.

**Implements**:
- `cpt-cf-mini-chat-algo-chat-management-chat-isolation`
- `cpt-cf-mini-chat-flow-chat-management-get-chat`
- `cpt-cf-mini-chat-flow-chat-management-list-chats`
- `cpt-cf-mini-chat-flow-chat-management-update-chat-title`
- `cpt-cf-mini-chat-flow-chat-management-delete-chat`

**Constraints**: `cpt-cf-mini-chat-principle-tenant-scoped`, `cpt-cf-mini-chat-principle-owner-only-content`

**Touches**:
- API: every route of this feature
- DB: `cpt-cf-mini-chat-dbtable-chats`
- Entities: `Chat`

### Chat Routes Registered, Deferred Scope Unimplemented

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-routes-and-deferred`

The system **MUST** register the five chat routes through the shared registration surface of the foundation under `url_prefix`, with the base license feature, the standard Problem error responses and the schemas `CreateChatReq`, `UpdateChatReq`, `ChatDetailDto`, `Page_ChatDetailDto` and `PageInfo` in the OpenAPI document, and **MUST NOT** implement the deferred scope.

- Routes: `POST /v1/chats`, `GET /v1/chats`, `GET /v1/chats/{id}`, `PATCH /v1/chats/{id}`, `DELETE /v1/chats/{id}`; the 415 missing-content-type response and `$select` are not declared in the OpenAPI.
- Not implemented in P1: `POST /v1/chats/{id}:temporary`, temporary chats with 24-hour cleanup (`is_temporary` is always `false`), group chats and chat sharing (ADR-0003), and the hard purge of soft-deleted rows.

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-create-chat`
- `cpt-cf-mini-chat-flow-chat-management-list-chats`
- `cpt-cf-mini-chat-state-chat-management-chat-lifecycle`

**Constraints**: `cpt-cf-mini-chat-constraint-license-gate`

**Touches**:
- API: `POST /v1/chats`, `GET /v1/chats`, `GET /v1/chats/{id}`, `PATCH /v1/chats/{id}`, `DELETE /v1/chats/{id}`
- DB: none
- Entities: `Chat`

### Test Layering, Traceability and Feature-Local Deviations

- [ ] `p1` - **ID**: `cpt-cf-mini-chat-dod-chat-management-tests-and-deviations`

The system **MUST** cover every acceptance-criteria item that this feature owns with tests in the layers below, and **MUST** record each feature-local deviation with its rationale and review owner.

**Test layering**:
- Unit tests live in the gear crate (`gears/mini-chat/mini-chat/`) and cover pure logic: title validation (absent, `null`, empty, whitespace-only, 1, 255 and 256 characters, trimming), model resolution against catalogs (supplied enabled, supplied disabled, unknown, default flag, first enabled fallback, tier ignored, no enabled entry), list query validation (every reason code, and `limit` absent, `1`, `100`, `101`, `0`, `-1`, `abc`, `1.5`, an empty value, an overflowing value such as `18446744073709551616`, a very large in-range value such as `18446744073709551615`, and `limit` together with `$top`, each asserting the exact HTTP status, `reason` and field stated in the list DoD), `ChatDetail` projection (title omitted, no internal fields), the activity ordering contract, and the delete transaction against an in-memory SQLite database including the rollback paths and the exact outbox payload.
- Integration and API tests exercise the five routes in-process through the router with stubbed platform clients (PDP, types-registry, OAGW, authn resolver) over SQLite; no network is used. They cover the 201 plus `Location`, get, list with filtering, ordering and pagination, rename, delete and the second delete, the Problem shapes (400, 404, 415, 422, 403, 503), and the two-tenant, two-user isolation scenarios. A stubbed model-policy plugin supplies the catalog.
- No tests are added under `testing/e2e/suites/mini_chat/`.
- Every implemented CDSL step carries `@cpt-begin` and `@cpt-end` markers with its ID, phase token and `inst-*` ID (traceability mode FULL).

**Reliability and data integrity**: the delete transaction is atomic (soft-delete, attachment marking and enqueue commit or roll back together), the soft-delete is conditional on `deleted_at IS NULL` so exactly one message is enqueued per chat, and the chat-cleanup message carries a stable `system_request_id`. **Security**: tenant and owner scoping, fail-closed authorization and 404 masking come from the foundation and are verified here. **Observability**: this feature adds no metric or audit event of its own (chat deletion is not audited, ADR-0009; cleanup metrics belong to the chat-cleanup handler), and uses the foundation's correlation and logging conventions, logging causes only and never titles or message content. **Rollback**: a failed transaction leaves no partial state; schema rollback is owned by the foundation migrations. **Compile-time gates**: those of the foundation apply (workspace lints, the domain-model lint on `Chat`, the example server build with the features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`).

**Feature-local deviations** (review owner for every item: the mini-chat gear owners who maintain `gears/mini-chat/docs/`):

| Deviation | Rationale | Source |
|-----------|-----------|--------|
| Delete soft-deletes the chat row only; messages, turns, reactions and attachment rows keep `deleted_at` empty and no hard purge exists | Every read goes through the chat, so the child rows are unreachable; the PRD retention wording (purge after a grace period) is not implemented in P1 | ADR-0009 |
| Delete emits no audit event and does not cancel a running turn | The audit content and deletion event are not implemented; a running generation is finalized normally and billed | ADR-0009 |
| `is_temporary` is always `false`; `POST /v1/chats/{id}:temporary` and the 24-hour temporary cleanup are absent | Deferred to P2 | DECOMPOSITION 2.2 |
| Group chats and chat sharing are absent | Deferred to P2+ | ADR-0003 |
| The title of an update is validated before the chat is loaded, as the DESIGN states for create | The DESIGN fixes the order only for create; the same order keeps validation independent of chat existence | DESIGN 3.3 |
| `message_count` is derived from the non-deleted messages of the chat instead of a stored column | The `chats` table defines no count column | DESIGN 3.7 |
| Where `config/mini-chat.yaml` and the DESIGN differ, the DESIGN governs | `gears/mini-chat/docs/` is the source of truth | DECOMPOSITION 1 |

**Implements**:
- `cpt-cf-mini-chat-flow-chat-management-create-chat`
- `cpt-cf-mini-chat-flow-chat-management-get-chat`
- `cpt-cf-mini-chat-flow-chat-management-list-chats`
- `cpt-cf-mini-chat-flow-chat-management-update-chat-title`
- `cpt-cf-mini-chat-flow-chat-management-delete-chat`

**Constraints**: `cpt-cf-mini-chat-constraint-model-locked-per-chat`, `cpt-cf-mini-chat-principle-tenant-scoped`

**Touches**:
- API: every route of this feature
- DB: `cpt-cf-mini-chat-dbtable-chats`, `cpt-cf-mini-chat-dbtable-attachments`, `cpt-cf-mini-chat-dbtable-messages`
- Entities: `Chat`

## 6. Acceptance Criteria

The first four items are the DECOMPOSITION 2.2 "Acceptance Criteria Covered" and the matching items of `acceptance-criteria.md`; each is both implemented and tested. The remaining items pin the details behind them.

- [ ] A chat's model is immutable once set: after creation no operation changes `chats.model`, a `PATCH` body that carries `model` renames the chat and returns the original `model`, and a chat whose model was disabled or removed from the catalog is still returned by get and list with its stored `model`.
- [ ] The create, get, list, update and delete lifecycle works for chats, including model and title validation: create returns 201 with the `ChatDetail` and `Location` `/mini-chat/v1/chats/{id}`; a title is trimmed and accepted at 1 to 255 characters and rejected with 400 `INVALID_TITLE` when empty, whitespace-only or 256 characters; a supplied disabled or unknown model gives 400 `INVALID_MODEL`; an absent model resolves to the first enabled `is_default` entry, else the first enabled entry; rename returns 200 with the new title and a newer `updated_at`; delete returns 204, then get and a second delete return 404.
- [ ] The list endpoint supports filtering, ordering and pagination, with validation of malformed input: `$filter` over `updated_at`, `id` and `title` with the allowed operators, `$orderby` over the six allowed values, `limit` default 20 with 100 as the clamped maximum, `0` rejected with 400 `INVALID_LIMIT` (field `$top`) and a negative, non-integer, empty or overflowing `limit` rejected with 400 `INVALID_QUERY_PARAMS` (field `query`), cursor pages without gaps or repeats, and every malformed filter, order, cursor, limit, select, unsupported option and query string returns 400 with `context.resource_type` equal to `gts.cf.core.odata.query.v1~` and the reason named in the list query validation algorithm (including `ORDER_WITH_CURSOR`, `ORDER_MISMATCH` and `FILTER_MISMATCH`).
- [ ] Chat ordering reflects the most recent activity: the default list order is `updated_at` descending with `id` tiebreaker; create, rename and delete set `updated_at`; a chat whose `updated_at` was moved forward by a write that follows the message-activity contract is listed before an older one.
- [ ] The chat detail shape is stable: `id`, `model`, `is_temporary` (`false`), `message_count`, `created_at` and `updated_at` are always present, `title` is omitted for an untitled chat, and `user_id`, `tenant_id` and `deleted_at` never appear.
- [ ] `message_count` equals the number of non-deleted messages of the chat on get, create (`0`), update and every list item, and a rename does not change it.
- [ ] Delete is atomic and enqueues the cleanup contract: one transaction sets `deleted_at`, marks the chat's attachments `cleanup_status = pending` and enqueues exactly one message in `mini-chat.chat_cleanup` (the configured `outbox.chat_cleanup_queue_name`) with `tenant_id`, `chat_id`, a stable `system_request_id`, `reason` equal to `chat_soft_delete` and `chat_deleted_at`; a forced enqueue failure leaves the chat active and the attachments unmarked; an oversized payload gives 400 `invalid_argument`; messages, turns and reactions are not modified; no audit event and no provider call occur.
- [ ] Tenant and owner isolation is enforced on every chat operation: with two tenants and two users in one tenant, a user reads, lists, updates and deletes only their own chats, a foreign or soft-deleted chat gives 404 with resource type `gts.cf.core.mini_chat.chat.v1~`, a PDP that returns only a tenant predicate still yields only the caller's chats, a PDP deny gives 403 `AUTHZ_DENIED`, and a PDP failure gives 503 with `Retry-After: 5`.
- [ ] Body and path errors follow the canonical contract: 415 `missing_json_content_type`, 400 `json_syntax_error`, 422 `invalid_json_body` (including a `PATCH` without `title`) and 400 `invalid_path_params` for a non-UUID id.
- [ ] The deferred scope stays unimplemented: `POST /v1/chats/{id}:temporary` is not registered, `is_temporary` is always `false`, and no group chat or sharing behavior exists.
- [ ] Unit tests live in the gear crate, integration tests run in-process over SQLite without a network, no test is added under `testing/e2e/suites/mini_chat/`, and every implemented CDSL step has `@cpt-begin` and `@cpt-end` markers.
